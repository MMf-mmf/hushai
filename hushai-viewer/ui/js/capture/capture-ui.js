// Capture modal: stream this browser's camera + mic into hushai, just like the Android app.
// A topbar button opens a modal with device pickers, a live preview, Start/Stop, and live
// upload status (including the device_id so you can find this stream in the NVR selector).
// All wire/transport logic lives in ./controller.js; this file is only DOM + glue.

import { CaptureController } from "../capture/controller.js";

function boot() {
  const openBtn = document.getElementById("btnCapture");
  const modal = document.getElementById("captureModal");
  if (!openBtn || !modal) return;

  const closeBtn = document.getElementById("captureClose");
  const preview = document.getElementById("capturePreview");
  const camSel = document.getElementById("captureCam");
  const micSel = document.getElementById("captureMic");
  const audioOnlyCb = document.getElementById("captureAudioOnly");
  const startBtn = document.getElementById("captureStart");
  const stopBtn = document.getElementById("captureStop");
  const recDot = document.getElementById("captureRecDot");
  const statusEl = document.getElementById("captureStatus");
  const errEl = document.getElementById("captureError");

  const controller = new CaptureController({
    onProgress: render,
    onError: showError,
    onState: render,
  });

  function showError(msg) {
    errEl.hidden = false;
    errEl.textContent = String(msg);
  }
  function clearError() {
    errEl.hidden = true;
    errEl.textContent = "";
  }

  function render(s) {
    const st = s || controller._state();
    recDot.classList.toggle("live", !!st.recording);
    startBtn.disabled = !!st.recording;
    stopBtn.disabled = !st.recording;
    camSel.disabled = !!st.recording;
    micSel.disabled = !!st.recording;
    audioOnlyCb.disabled = !!st.recording;

    statusEl.replaceChildren();
    const rows = [
      ["Device id", st.deviceId || controller.deviceId],
      ["Stream", st.streamId || "—"],
      ["Captured", st.built ?? 0],
      ["Uploaded", st.uploaded ?? 0],
      ["Queued", st.depth ?? 0],
      ["Last status", st.lastStatus ?? "—"],
    ];
    for (const [k, v] of rows) {
      const row = document.createElement("div");
      row.className = "capture-stat";
      const key = document.createElement("span");
      key.className = "muted";
      key.textContent = k;
      const val = document.createElement("span");
      val.className = "mono";
      val.textContent = String(v);
      row.append(key, val);
      statusEl.appendChild(row);
    }
  }

  function fill(sel, devices, kind) {
    const cur = sel.value;
    sel.replaceChildren();
    const def = document.createElement("option");
    def.value = "";
    def.textContent = `Default ${kind.toLowerCase()}`;
    sel.appendChild(def);
    devices.forEach((d, i) => {
      const o = document.createElement("option");
      o.value = d.deviceId;
      o.textContent = d.label || `${kind} ${i + 1}`;
      sel.appendChild(o);
    });
    if (cur) sel.value = cur;
  }

  async function populateDevices() {
    try {
      const { cameras, mics } = await controller.listDevices();
      fill(camSel, cameras, "Camera");
      fill(micSel, mics, "Microphone");
    } catch {
      /* ignore — labels just stay generic */
    }
  }

  function friendlyError(e) {
    const name = e?.name || "";
    if (name === "NotAllowedError" || name === "SecurityError")
      return "Camera/microphone permission denied. Allow access, then click Start again.";
    if (name === "NotFoundError") return "No camera or microphone found.";
    if (name === "NotReadableError")
      return "The camera/microphone is already in use by another app.";
    return e?.message || String(e);
  }

  async function open() {
    clearError();
    modal.hidden = false;
    if (!CaptureController.isSupported(audioOnlyCb.checked)) {
      showError(
        "This browser can't record H.264/AAC MP4 (or this isn't a secure origin). Use Chrome/Edge 130+ or Safari on http://127.0.0.1.",
      );
    }
    await populateDevices();
    render();
  }
  function close() {
    modal.hidden = true;
  }

  startBtn.addEventListener("click", async () => {
    clearError();
    startBtn.disabled = true;
    try {
      const res = await controller.start({
        audioOnly: audioOnlyCb.checked,
        videoDeviceId: camSel.value || undefined,
        audioDeviceId: micSel.value || undefined,
      });
      if (res?.stream) preview.srcObject = res.stream;
      await populateDevices(); // labels are populated now that permission is granted
      render();
    } catch (e) {
      showError(friendlyError(e));
      render();
    }
  });

  stopBtn.addEventListener("click", () => {
    controller.stop();
    preview.srcObject = null;
    render();
  });

  openBtn.addEventListener("click", open);
  if (closeBtn) closeBtn.addEventListener("click", close);
  modal.addEventListener("click", (e) => {
    if (e.target === modal) close(); // click the backdrop to dismiss
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !modal.hidden) close();
  });

  // Warn before leaving if we're still capturing or have unsent segments buffered.
  window.addEventListener("beforeunload", (e) => {
    if (controller.recording || (controller.queue && controller.queue.depth > 0)) {
      e.preventDefault();
      e.returnValue = "";
    }
  });
  navigator.mediaDevices?.addEventListener?.("devicechange", populateDevices);
}

boot();
