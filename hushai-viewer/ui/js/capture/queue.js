// Bounded, single-flight store-and-forward queue for finalized segments. In-memory only for
// v1 (a closed tab can't keep capturing anyway). On overflow we drop the OLDEST unsent
// segment and remember its stream, so the next segment built/sent on that stream sets
// gap_before — keeping the timeline honest about the dropped data (mirrors the Android
// DurableSegmentBuffer's gap bookkeeping, simplified).

const MAX_QUEUE = 200; // ~6–7 minutes of 2s segments before we start shedding oldest

export class SegmentQueue {
  constructor() {
    this.items = [];
    this.gapPending = new Set(); // stream_ids that dropped a segment and owe a gap mark
  }
  get depth() {
    return this.items.length;
  }

  // Whether the NEXT segment on `streamId` should carry gap_before (clears the flag).
  consumeGap(streamId) {
    if (this.gapPending.has(streamId)) {
      this.gapPending.delete(streamId);
      return true;
    }
    return false;
  }

  markGap(streamId) {
    this.gapPending.add(streamId);
  }

  enqueue(item) {
    this.items.push(item);
    while (this.items.length > MAX_QUEUE) {
      const dropped = this.items.shift();
      this.markGap(dropped.streamId);
    }
  }
  peek() {
    return this.items[0] ?? null;
  }
  shift() {
    return this.items.shift() ?? null;
  }
}
