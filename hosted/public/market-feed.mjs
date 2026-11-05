// One selected market: initial HTTP snapshot, stream updates, bounded fallback.
export function subscribeMarket({ id, read, connect, onData, onStatus, clock = globalThis }) {
  let stopped = false,
    socket,
    reconnect,
    fetching = false;
  let height = -1,
    lastPush = 0;
  const apply = (value, pushed) => {
    if (stopped || value?.error || value?.market_id !== id || !Number.isSafeInteger(value.height))
      return;
    if (value.height < height) return; // A slower HTTP request cannot regress a streamed book.
    height = value.height;
    if (pushed) lastPush = clock.Date.now();
    onData(value);
    onStatus(lastPush && clock.Date.now() - lastPush < 5000 ? "live" : "snapshot");
  };
  async function refresh() {
    if (stopped || fetching) return;
    fetching = true;
    try {
      apply(await read(), false);
    } catch {
      if (!stopped)
        onStatus(
          height < 0
            ? "unavailable"
            : lastPush && clock.Date.now() - lastPush < 5000
              ? "live"
              : "snapshot",
        );
    } finally {
      fetching = false;
    }
  }
  function listen() {
    if (stopped) return;
    try {
      socket = connect();
      socket.onmessage = (event) => {
        try {
          const value = JSON.parse(event.data);
          if (value.error) {
            void refresh();
            return;
          }
          apply(value, true);
        } catch {
          void refresh();
        }
      };
      socket.onclose = () => {
        if (stopped) return;
        lastPush = 0;
        onStatus(height < 0 ? "connecting" : "snapshot");
        void refresh();
        reconnect = clock.setTimeout(listen, 3000);
      };
    } catch {
      onStatus(height < 0 ? "connecting" : "snapshot");
      reconnect = clock.setTimeout(listen, 3000);
    }
  }
  onStatus("connecting");
  listen();
  const ready = refresh();
  const fallback = clock.setInterval(() => {
    if (!lastPush || clock.Date.now() - lastPush >= 5000) void refresh();
  }, 5000);
  return {
    ready,
    refresh,
    stop() {
      stopped = true;
      clock.clearTimeout(reconnect);
      clock.clearInterval(fallback);
      if (socket) {
        socket.onmessage = null;
        socket.onclose = null;
        socket.close();
      }
    },
  };
}
