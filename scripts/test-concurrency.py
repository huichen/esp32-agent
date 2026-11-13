#!/usr/bin/env python3
"""Real-server concurrency regression; works with a simulator or the bench board.

No credentials are read from files. --pin is optional for a configured portal.
--agent temporarily points the model at the BOARD'S OWN selftest endpoint, keeps
its key untouched, and restores the three public settings in finally. It adds one
explicit test turn; it does not clear any history or change WiFi/manifest settings.
"""
import argparse
import base64
import concurrent.futures
import json
import statistics
import time
import urllib.request


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--host', default='http://192.168.3.7')
    p.add_argument('--pin', default='')
    p.add_argument('--soak', type=int, default=0)
    p.add_argument('--agent', action='store_true')
    p.add_argument('--max-ms', type=int, default=1000)
    args = p.parse_args()
    host = args.host.rstrip('/')
    latencies = []
    checks = 0

    def request(path, body=None, timeout=40):
        nonlocal checks
        data = None if body is None else json.dumps(body).encode()
        headers = {'Content-Type': 'application/json', 'Connection': 'close'}
        if args.pin:
            headers['X-Portal-Pin'] = args.pin
        req = urllib.request.Request(host + path, data=data, headers=headers)
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
        checks += 1
        return json.loads(raw)

    def control(path='/api/status'):
        start = time.monotonic()
        result = request(path, timeout=args.max_ms / 1000 + 1)
        elapsed = (time.monotonic() - start) * 1000
        latencies.append(elapsed)
        assert elapsed < args.max_ms, (path, elapsed)
        assert result.get('snapshot') is True, result
        return result

    def tool(name, arguments):
        response = request('/mcp', {'jsonrpc': '2.0', 'id': 9101,
            'method': 'tools/call', 'params': {'name': name, 'arguments': arguments}})
        assert 'error' not in response, response
        result = response['result']
        assert not result.get('isError'), result
        return result.get('structuredContent', result)

    initial = control()
    firmware = initial['firmware']['commit']
    assert not initial['agent'].get('run_active'), 'Do not start the test over a live user turn'

    def exercise(name, calls):
        before = len(latencies)
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(calls)) as pool:
            futures = [pool.submit(tool, tool_name, arguments) for tool_name, arguments in calls]
            time.sleep(0.25)
            while not all(f.done() for f in futures):
                control('/api/status')
                control('/healthz')
                time.sleep(0.1)
            for future in futures:
                future.result()
        assert len(latencies) > before, 'Tool did not stay in flight for the test'
        print(name, 'control_reads=', len(latencies)-before,
              'max_ms=', round(max(latencies[before:]), 1), flush=True)

    exercise('shell-sleep', [('shell', {'command': 'sleep 5'})])
    exercise('script-sleep-and-nested-tool', [('script', {'source':
        'sleep(5000)\ntool("shell", {command: "echo concurrency"})'})])
    exercise('two-serialized-tools', [('shell', {'command': 'sleep 4'}),
        ('script', {'source': 'sleep(4000)\n1'})])
    clip = '_mutex_probe'
    tool('clip_new', {'name': clip, 'kind': 'pcm', 'bytes': 48000, 'rate_hz': 24000})
    try:
        tool('clip_append', {'name': clip, 'data_b64': base64.b64encode(bytes(48000)).decode()})
        exercise('silent-pcm-playback', [('clip_play', {'name': clip, 'loops': 5})])
    finally:
        tool('clip_drop', {'name': clip})

    if args.agent:
        cfg = request('/api/agent/config')
        saved = {key: cfg[key] for key in ('base_url', 'model', 'protocol')}
        try:
            # The existing key never leaves the board for a new host.
            request('/api/agent/config', {'base_url': host + '/selftest/chat/completions',
                'model': 'selftest', 'protocol': 'chat'})
            request('/api/agent/message', {'text': 'concurrency regression: run the scripted test'})
            deadline = time.monotonic() + 35
            while time.monotonic() < deadline:
                s = control()
                if s['agent'].get('run_active') and s['age_ms'] > 1000:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError('Agent never entered the long locked tool')
            start = time.monotonic()
            cancelled = request('/api/agent/cancel', {}, timeout=2)
            accepted_ms = (time.monotonic()-start)*1000
            assert cancelled.get('cancelling') is True, cancelled
            while time.monotonic()-start < 3:
                if not control()['agent'].get('run_active'):
                    break
                time.sleep(0.05)
            else:
                raise AssertionError('Cancelled script did not stop within 3 seconds')
            history = request('/api/agent/history')
            print('agent-interrupt accepted_ms=', round(accepted_ms,1),
                  'stopped_ms=', round((time.monotonic()-start)*1000,1), flush=True)
            assert history, 'History remains available after interrupt'
        finally:
            request('/api/agent/config', saved)
            restored = request('/api/agent/config')
            assert all(restored[k] == v for k, v in saved.items()), 'Model settings were not restored'

    end = time.monotonic() + args.soak
    last = control()['uptime_s']
    while time.monotonic() < end:
        s = control()
        assert s['uptime_s'] >= last, 'Unexpected board reset'
        assert s['firmware']['commit'] == firmware, 'Firmware changed during test'
        assert s['agent']['thread'] == 'running', s['agent']
        last = s['uptime_s']
        control('/healthz')
        time.sleep(1)
    final = control()
    samples = sorted(latencies)
    print(json.dumps({'firmware': firmware, 'checks': checks, 'control_reads': len(samples),
        'median_ms': round(statistics.median(samples),1),
        'p95_ms': round(samples[int((len(samples)-1)*0.95)],1),
        'max_ms': round(max(samples),1), 'uptime_s': final['uptime_s'],
        'soak_s': args.soak}, indent=2), flush=True)


if __name__ == '__main__':
    main()
