import https from 'node:https';

// The League client and the in-game Live Client API serve a Riot self-signed cert on 127.0.0.1 only.
// Certificate checks are disabled for these loopback requests exclusively.
const loopbackAgent = new https.Agent({ rejectUnauthorized: false, keepAlive: true });

export function loopbackJson<T>(port: number, path: string, auth?: string, timeoutMs = 2500): Promise<T> {
  return new Promise((resolve, reject) => {
    const req = https.request(
      { host: '127.0.0.1', port, path, method: 'GET', agent: loopbackAgent, timeout: timeoutMs,
        headers: { Accept: 'application/json', ...(auth ? { Authorization: auth } : {}) } },
      (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (body += c));
        res.on('end', () => {
          if ((res.statusCode ?? 500) >= 400) return reject(new Error(`HTTP ${res.statusCode} ${path}`));
          try { resolve(JSON.parse(body) as T); } catch (e) { reject(e); }
        });
      },
    );
    req.on('timeout', () => req.destroy(new Error('timeout')));
    req.on('error', reject);
    req.end();
  });
}

/**
 * A write to the League client (Stage C import: rune page, item set). Same loopback rules as above; an empty answer
 * (204) resolves to null.
 */
export function loopbackRequest<T>(port: number, method: 'GET' | 'POST' | 'PUT' | 'DELETE', path: string, auth: string,
  body?: unknown, timeoutMs = 5000): Promise<T | null> {
  const payload = body === undefined ? undefined : JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const req = https.request(
      { host: '127.0.0.1', port, path, method, agent: loopbackAgent, timeout: timeoutMs,
        headers: { Accept: 'application/json', Authorization: auth,
          ...(payload !== undefined ? { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(payload) } : {}) } },
      (res) => {
        let text = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (text += c));
        res.on('end', () => {
          if ((res.statusCode ?? 500) >= 400) {
            let detail = '';
            try { detail = (JSON.parse(text) as { message?: string }).message ?? ''; } catch { /* not JSON */ }
            return reject(new Error(`HTTP ${res.statusCode} ${method} ${path}${detail ? `: ${detail}` : ''}`));
          }
          if (!text.trim()) return resolve(null);
          try { resolve(JSON.parse(text) as T); } catch (e) { reject(e); }
        });
      },
    );
    req.on('timeout', () => req.destroy(new Error('timeout')));
    req.on('error', reject);
    req.end(payload);
  });
}
