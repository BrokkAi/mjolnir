// One device-local owner shared by viewer tabs and the service worker.
// Keep endpoints until the daemon acknowledges replacement/removal, so network
// and authentication failures can be retried by the next reconcile trigger.
globalThis.MjPush = (() => {
  async function store(value) {
    const db = await new Promise((resolve, reject) => {
      const request = indexedDB.open('mj-notifications', 1);
      request.onupgradeneeded = () => request.result.createObjectStore('device');
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    try {
      return await new Promise((resolve, reject) => {
        const tx = db.transaction('device', value === undefined ? 'readonly' : 'readwrite');
        const request = value === undefined ? tx.objectStore('device').get('state') : tx.objectStore('device').put(value, 'state');
        tx.oncomplete = () => resolve(value === undefined ? request.result || { enabled: false, endpoints: [] } : value);
        tx.onerror = tx.onabort = () => reject(tx.error || request.error);
      });
    } finally { db.close(); }
  }
  const locked = work => navigator.locks.request('mj-notifications', work);
  async function action(body) {
    const response = await fetch('/api/actions', { method: 'POST', credentials: 'include', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
    if (!response.ok) throw new Error(`Notification sync failed (HTTP ${response.status}). Check sign-in and try again.`);
  }
  async function reconcileLocked(registration, oldEndpoint) {
    const state = await store();
    let subscription = await registration.pushManager.getSubscription();
    state.endpoints = [...new Set([...state.endpoints, oldEndpoint, subscription?.endpoint].filter(Boolean))];
    await store(state);
    if (state.enabled && Notification.permission === 'granted') {
      if (!subscription) {
        const response = await fetch('/api/push/key', { credentials: 'include' });
        if (!response.ok) throw new Error(`Notification key unavailable (HTTP ${response.status}). Check sign-in and try again.`);
        const { public_key: key } = await response.json();
        subscription = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: Uint8Array.from(atob(key.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0)) });
      }
      // The previous endpoint remains durable until atomic renewal completes.
      for (const endpoint of state.endpoints.filter(endpoint => endpoint !== subscription.endpoint)) {
        await action({ action: 'push-renew', old_endpoint: endpoint, subscription: subscription.toJSON() });
      }
      await action({ action: 'push-subscribe', subscription: subscription.toJSON() });
      state.endpoints = [subscription.endpoint];
      await store(state);
      return true;
    }
    for (const endpoint of state.endpoints) await action({ action: 'push-unsubscribe', endpoint });
    if (subscription && !await subscription.unsubscribe()) throw new Error('Could not disable notifications. Try again.');
    state.endpoints = [];
    await store(state);
    return false;
  }
  return {
    state: () => locked(() => store()),
    dismiss: () => locked(async () => { const state = await store(); state.dismissed = true; await store(state); }),
    reconcile: (registration, oldEndpoint) => locked(() => reconcileLocked(registration, oldEndpoint)),
    setEnabled: (enabled, registration) => locked(async () => {
      const state = await store();
      state.enabled = enabled;
      await store(state);
      return reconcileLocked(registration);
    }),
  };
})();
