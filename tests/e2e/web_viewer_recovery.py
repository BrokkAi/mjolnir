#!/usr/bin/env python3
"""Exercise HTTPS port recovery with disposable daemons; requires Linux and openssl.

Build first with cargo build -p brokk-mjolnir, then run:
    python3 tests/e2e/web_viewer_recovery.py --mj target/debug/mj
No existing daemon or user configuration is read or modified.
"""

import argparse
import json
import os
import pathlib
import select
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.request

from reliability_lab import daemon_request


def request(instance, action, arguments=None):
    operation = {'action': action}
    if arguments is not None:
        operation['arguments'] = arguments
    return daemon_request(instance['data'], operation)


def wait_state(instance, name):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        access = request(instance, 'web_viewer_access')['value']
        if name in access:
            return access[name]
        if 'Unavailable' in access:
            raise RuntimeError(access['Unavailable'])
        time.sleep(0.1)
    raise RuntimeError(f'did not reach {name}')


def https_get(url):
    # The certificate is generated solely for these isolated local test daemons.
    context = ssl._create_unverified_context()
    with urllib.request.urlopen(url, context=context, timeout=5) as response:
        assert response.status == 200
        assert b'<!doctype html>' in response.read().lower()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mj', type=pathlib.Path, required=True)
    args = parser.parse_args()
    if sys.platform != 'linux' or not hasattr(os, 'pidfd_open'):
        parser.error('This test requires Linux process handles for safe daemon cleanup.')
    binary = args.mj.resolve(strict=True)
    repo = pathlib.Path(__file__).resolve().parents[2]
    (repo / 'target').mkdir(exist_ok=True)
    root = pathlib.Path(tempfile.mkdtemp(prefix='viewer-recovery-', dir=repo / 'target'))
    instances = []

    def start(name, port):
        config = root / name / 'config'
        data = root / name / 'data'
        config.mkdir(parents=True)
        (config / 'config.toml').write_text(f'''version = 1
    [phone]
    bind = "127.0.0.1:{port}"
    tailscale_detect = false
    tls_cert = "{root / 'cert.pem'}"
    tls_key = "{root / 'key.pem'}"
    ''')
        env = dict(os.environ, MJ_CONFIG_DIR=str(config), MJ_DATA_DIR=str(data))
        subprocess.run(
            [str(binary), 'daemon', 'restart'], env=env, check=True,
            capture_output=True, text=True, timeout=20,
        )
        metadata = json.loads((data / 'daemon.json').read_text())
        instance = {'data': data, 'env': env, 'pid': metadata['pid'], 'pidfd': os.pidfd_open(metadata['pid'])}
        instances.append(instance)
        return instance

    try:
        subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',str(root / 'key.pem'),
                        '-out',str(root / 'cert.pem'),'-days','1','-subj','/CN=localhost'],check=True,capture_output=True,timeout=15)
        with socket.socket() as reserved:
            reserved.bind(('127.0.0.1', 0))
            port = reserved.getsockname()[1]
        owner = start('owner', port)
        first = wait_state(owner, 'Ready')
        https_get(first['viewer_url'])
        contender = start('contender', port)
        failed = wait_state(contender, 'Failed')
        assert failed['address'] == f'127.0.0.1:{port}' and failed['port_conflict']
        processes = request(contender, 'inspect_web_listener')['value']
        identified = next(p for p in processes if p['pid'] == owner['pid'])
        assert identified['stop_disabled_reason'] is None, identified
        assert request(contender, 'status')['value']['phone_status']['state'] == 'error'
        print('PASS: occupied HTTPS port reports Failed and inspection identifies the owning Mjolnir daemon')
        request(contender, 'recover_web_viewer', {'StopAndRetry': identified})
        recovered = wait_state(contender, 'Ready')
        assert recovered['viewer_url'] == first['viewer_url']
        https_get(recovered['viewer_url'])
        poller = select.poll()
        poller.register(owner['pidfd'], select.POLLIN)
        assert poller.poll(12000), 'confirmed server did not terminate'
        assert request(contender, 'status')['value']['pid'] == contender['pid']
        print('PASS: StopAndRetry stops only the confirmed daemon and serves HTTPS on the original port')
        alternate = start('alternate', port)
        wait_state(alternate, 'Failed')
        request(alternate, 'recover_web_viewer', 'AnotherPort')
        alternate_access = wait_state(alternate, 'Ready')
        assert alternate_access['viewer_url'] != recovered['viewer_url']
        assert alternate_access['viewer_url'].startswith('https://127.0.0.1:')
        https_get(alternate_access['viewer_url'])
        https_get(recovered['viewer_url'])
        assert request(contender, 'status')['value']['pid'] == contender['pid']
        print('PASS: AnotherPort serves HTTPS at the new URL while the existing server remains available')
    finally:
        cleanup_ok = True
        for instance in reversed(instances):
            poller = select.poll()
            poller.register(instance['pidfd'], select.POLLIN)
            if not poller.poll(0):
                result = subprocess.run(
                    [str(binary), 'daemon', 'stop'], env=instance['env'],
                    capture_output=True, text=True, timeout=15,
                )
                if result.returncode:
                    cleanup_ok = False
                    print('Cleanup failed for test daemon PID',instance['pid'],result.stderr)
                if not poller.poll(12000):
                    cleanup_ok = False
                    print('Test daemon remains active; preserving files:',instance['data'])
            os.close(instance['pidfd'])
        if cleanup_ok:
            shutil.rmtree(root)
        else:
            raise RuntimeError('Test cleanup did not finish; files retained at '+str(root))


if __name__ == "__main__":
    main()
