// Capture controller: the public façade the modal UI talks to. Owns the getUserMedia stream
// lifecycle and wires recorder → segment builder → queue → uploader, exposing progress/state.

import { Recorder, pickMime } from "./recorder.js";
import { Session, getDeviceId } from "./identity.js";
import { buildSegment } from "./segment.js";
import { SegmentQueue } from "./queue.js";
import { Uploader } from "./upload.js";

export class CaptureController {
  constructor({ onProgress, onError, onState } = {}) {
    this.onProgress = onProgress;
    this.onError = onError;
    this.onState = onState;
    this.deviceId = getDeviceId();
    this.stream = null;
    this.recorder = null;
    this.session = null;
    this.queue = null;
    this.uploader = null;
    this.recording = false;
    this.built = 0;
  }

  // True when this browser can both capture (getUserMedia) and record the NVR-playable
  // H.264/AAC MP4 format. False on Firefox / insecure origins.
  static isSupported(audioOnly = false) {
    return !!navigator.mediaDevices?.getUserMedia && !!pickMime(audioOnly);
  }

  async listDevices() {
    if (!navigator.mediaDevices?.enumerateDevices) return { cameras: [], mics: [] };
    const devices = await navigator.mediaDevices.enumerateDevices();
    return {
      cameras: devices.filter((d) => d.kind === "videoinput"),
      mics: devices.filter((d) => d.kind === "audioinput"),
    };
  }

  async start({ audioOnly = false, videoDeviceId, audioDeviceId } = {}) {
    if (this.recording) return null;
    if (!CaptureController.isSupported(audioOnly)) {
      throw new Error(
        "This browser can't record H.264/AAC MP4. Use Chrome/Edge 130+ or Safari.",
      );
    }

    const audio = { channelCount: 1, sampleRate: 16000, echoCancellation: true };
    if (audioDeviceId) audio.deviceId = { exact: audioDeviceId };
    const constraints = { audio };
    if (!audioOnly) {
      const video = {
        width: { ideal: 1280 },
        height: { ideal: 720 },
        frameRate: { ideal: 15 },
      };
      if (videoDeviceId) video.deviceId = { exact: videoDeviceId };
      constraints.video = video;
    }

    this.stream = await navigator.mediaDevices.getUserMedia(constraints);
    this.session = new Session(this.deviceId, audioOnly);
    this.queue = new SegmentQueue();
    this.built = 0;
    this.uploader = new Uploader(this.queue, {
      onProgress: () => this.onProgress?.(this._state()),
      onError: (msg) => this.onError?.(msg),
    });

    // Declare the actual (browser-chosen) audio settings; sampleRate is a hint the encoder
    // often ignores, and the worker resamples server-side, so we just record what we got.
    const settings = this.stream.getAudioTracks()[0]?.getSettings?.() || {};
    const attrs = {};
    if (settings.sampleRate) attrs.audio_sample_rate = String(settings.sampleRate);
    if (settings.channelCount) attrs.audio_channels = String(settings.channelCount);

    this.recorder = new Recorder({
      stream: this.stream,
      audioOnly,
      onSegment: async (raw) => {
        try {
          const gapBefore = this.queue.consumeGap(this.session.streamId);
          const seg = await buildSegment({
            raw,
            session: this.session,
            audioOnly,
            gapBefore,
            attrs,
          });
          this.queue.enqueue(seg);
          this.built++;
          this.onProgress?.(this._state());
        } catch (e) {
          this.onError?.("segment build failed: " + (e?.message || e));
        }
      },
      onError: (e) => this.onError?.(e?.message || String(e)),
      onStopped: () => {
        // Recorder fully stopped (final segment flushed) → release the camera/mic hardware.
        this._releaseStream();
        this.onState?.(this._state());
      },
    });

    this.recording = true;
    this.uploader.start();
    this.recorder.start();
    this.onState?.(this._state());
    return { deviceId: this.deviceId, streamId: this.session.streamId, stream: this.stream };
  }

  // Stop capturing. The recorder flushes its final segment, then onStopped releases the
  // stream. The uploader keeps draining buffered segments in the background.
  stop() {
    if (!this.recording) return;
    this.recording = false;
    if (this.recorder) this.recorder.stop();
    this.onState?.(this._state());
  }

  // Hard stop (e.g. before leaving the page): also halt the uploader.
  shutdown() {
    this.recording = false;
    if (this.recorder) this.recorder.stop();
    this._releaseStream();
    if (this.uploader) this.uploader.stop();
  }

  _releaseStream() {
    if (this.stream) {
      this.stream.getTracks().forEach((t) => {
        try {
          t.stop();
        } catch {
          /* ignore */
        }
      });
    }
  }

  _state() {
    return {
      recording: this.recording,
      built: this.built,
      uploaded: this.uploader?.uploaded ?? 0,
      depth: this.queue?.depth ?? 0,
      lastStatus: this.uploader?.lastStatus ?? null,
      deviceId: this.deviceId,
      streamId: this.session?.streamId ?? null,
    };
  }
}
