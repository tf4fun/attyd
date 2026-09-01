export function limitNdjsonLineBytes(
  input: ReadableStream<Uint8Array>,
  maximum: number,
): ReadableStream<Uint8Array> {
  if (!Number.isSafeInteger(maximum) || maximum <= 0) {
    throw new Error("NDJSON line limit must be a positive integer");
  }
  let pendingBytes = 0;
  return input.pipeThrough(new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      let start = 0;
      while (start < chunk.length) {
        const newline = chunk.indexOf(10, start);
        const end = newline < 0 ? chunk.length : newline;
        pendingBytes += end - start;
        if (pendingBytes > maximum) {
          throw new Error(`Agent NDJSON line exceeds ${maximum} bytes`);
        }
        if (newline < 0) break;
        pendingBytes = 0;
        start = newline + 1;
      }
      controller.enqueue(chunk);
    },
  }));
}
