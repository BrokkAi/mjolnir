#!/usr/bin/env python3
"""Register a new workspace crate on crates.io so the release workflow can
publish it.

`.github/workflows/publish.yml` authenticates through crates.io Trusted
Publishing, which can only be configured on a crate that already exists. This
script does the one-time setup, in this order:

1. publishes a `0.0.0` name-reservation crate when the name is free;
2. adds the standard owners that are missing;
3. adds the Trusted Publishing configuration for `publish.yml`;
4. restricts the crate to Trusted Publishing, which is why it comes last: the
   placeholder is the only release ever published with a personal token.

Every step first reads the current state and skips what is already done, so
a failed run can be repeated. `--check` only reports and changes nothing.

The token comes from `CARGO_REGISTRY_TOKEN`, or from `cargo login`'s
`credentials.toml`. It needs the scopes `publish-new`, `change-owners` and
`trusted-publishing`, and its user must be a member of the owning team.

    scripts/register-crate.py brokk-mj-store --description "Mjolnir session store"
    scripts/register-crate.py --check brokk-mj-store
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request

API = 'https://crates.io/api/v1'
# crates.io rejects requests without a User-Agent.
USER_AGENT = 'register-crate.py'

# Owners of every published Mjolnir crate. A user receives an invitation and
# must accept it; the team is added at once.
OWNERS = ['foundev', 'jbellis', 'DavidBakerEffendi', 'github:brokkai:brokk-eng']

# Must match the `publish` job of .github/workflows/publish.yml: crates.io
# compares these with the claims of the workflow's OIDC token.
PUBLISHER = {
    'repository_owner': 'BrokkAi',
    'repository_name': 'mjolnir',
    'workflow_filename': 'publish.yml',
    'environment': 'crates-io',
}

REPOSITORY = 'https://github.com/BrokkAi/mjolnir'
LICENSE = 'GPL-3.0-only'


class ApiError(Exception):
    pass


def request(method, path, token=None, body=None):
    """Returns the decoded response, or None for 404."""
    headers = {'User-Agent': USER_AGENT, 'Accept': 'application/json'}
    data = None
    if token:
        headers['Authorization'] = token
    if body is not None:
        headers['Content-Type'] = 'application/json'
        data = json.dumps(body).encode()
    req = urllib.request.Request(API + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404 and method == 'GET':
            return None
        text = error.read().decode(errors='replace')
        try:
            text = '; '.join(e['detail'] for e in json.loads(text)['errors'])
        except (ValueError, KeyError, TypeError):
            pass
        raise ApiError(f'{method} {path} failed with HTTP {error.code}: {text}') from None
    except urllib.error.URLError as error:
        raise ApiError(f'{method} {path} failed: {error.reason}') from None


def registry_token():
    token = os.environ.get('CARGO_REGISTRY_TOKEN')
    if token:
        return token
    cargo_home = os.environ.get('CARGO_HOME') or os.path.join(os.path.expanduser('~'), '.cargo')
    for name in ('credentials.toml', 'credentials'):
        try:
            with open(os.path.join(cargo_home, name), 'rb') as file:
                return tomllib.load(file)['registry']['token']
        except (FileNotFoundError, KeyError):
            continue
    return None


def owner_key(login):
    # crates.io pads and lower-cases team logins in its replies.
    return login.strip().lower()


def current_owners(name):
    return [owner_key(o['login']) for o in request('GET', f'/crates/{name}/owners')['users']]


def placeholder(directory, name, description):
    manifest = '\n'.join([
        '[package]',
        f'name = {json.dumps(name)}',
        'version = "0.0.0"',
        'edition = "2024"',
        f'license = {json.dumps(LICENSE)}',
        f'description = {json.dumps(description + " (name reservation; released with Mjolnir)")}',
        f'repository = {json.dumps(REPOSITORY)}',
        '',
    ])
    with open(os.path.join(directory, 'Cargo.toml'), 'w') as file:
        file.write(manifest)
    os.mkdir(os.path.join(directory, 'src'))
    with open(os.path.join(directory, 'src', 'lib.rs'), 'w') as file:
        file.write(f'//! Name reservation for `{name}`, which is released with Mjolnir:\n//! <{REPOSITORY}>.\n')


def publish_placeholder(name, description, token, dry_run):
    with tempfile.TemporaryDirectory() as directory:
        placeholder(directory, name, description)
        # `--no-verify`: the crate has no code to compile, so nothing is built.
        command = ['cargo', 'publish', '--no-verify']
        env = dict(os.environ)
        if dry_run:
            command.append('--dry-run')
        else:
            env['CARGO_REGISTRY_TOKEN'] = token
        result = subprocess.run(command, cwd=directory, env=env)
        if result.returncode != 0:
            raise ApiError(f'`{" ".join(command)}` exited with status {result.returncode}')


def main():
    sys.stdout.reconfigure(line_buffering=True)
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('name', help='package name, for example brokk-mj-store')
    parser.add_argument('--description', help='one line for the crates.io page; required when the name is not registered yet')
    parser.add_argument('--check', action='store_true', help='report what is missing and change nothing')
    args = parser.parse_args()

    name = args.name
    if not re.fullmatch(r'brokk-mj[a-z0-9-]*', name):
        parser.error(f'{name!r} does not follow the `brokk-mj-*` package naming of this workspace')

    token = registry_token()
    if not token and not args.check:
        sys.exit('no crates.io token: set CARGO_REGISTRY_TOKEN or run `cargo login` (scopes: publish-new, change-owners, trusted-publishing)')

    missing = []

    def step(done, label, apply):
        if done:
            print(f'ok       {label}')
        elif args.check:
            print(f'MISSING  {label}')
            missing.append(label)
        else:
            apply()
            print(f'done     {label}')

    # 1. The crate itself.
    crate = request('GET', f'/crates/{name}')
    if crate is None:
        if not args.description:
            parser.error(f'{name} is not on crates.io yet: pass --description for its placeholder')
        step(False, f'{name} exists on crates.io',
             lambda: publish_placeholder(name, args.description, token, dry_run=False))
        if args.check:
            # Nothing else can be read for a crate that does not exist. Prove
            # the placeholder packages, so the real run fails only on access.
            publish_placeholder(name, args.description, None, dry_run=True)
            for label in ('standard owners', 'Trusted Publishing configuration', 'Trusted Publishing only'):
                print(f'MISSING  {label}')
            sys.exit(1)
        crate = request('GET', f'/crates/{name}')
        if crate is None:
            sys.exit(f'{name} was published but crates.io does not list it yet; run the script again')
        owners = current_owners(name)
    else:
        owners = current_owners(name)
        if not set(owners) & {owner_key(o) for o in OWNERS}:
            sys.exit(f'{name} is already on crates.io and owned by {", ".join(owners)}: choose another name')
        step(True, f'{name} exists on crates.io', None)

    # 2. Owners. The publisher of the placeholder already is one.
    absent = [o for o in OWNERS if owner_key(o) not in owners]
    def add_owners():
        print('         ' + request('PUT', f'/crates/{name}/owners', token, {'owners': absent})['msg'])
    step(not absent, 'standard owners' + (f' (to add: {", ".join(absent)})' if absent else ''), add_owners)

    # 3. Trusted Publishing configuration. Listing it needs a token, so
    # without one the state is unknown, which is not the same as absent.
    if token:
        query = urllib.parse.urlencode({'crate': name})
        configs = request('GET', f'/trusted_publishing/github_configs?{query}', token)['github_configs']
        def matches(config):
            return all((config.get(key) or '').lower() == value.lower() for key, value in PUBLISHER.items())
        step(any(matches(c) for c in configs),
             'Trusted Publishing configuration for {repository_owner}/{repository_name}, {workflow_filename}, environment {environment}'.format(**PUBLISHER),
             lambda: request('POST', '/trusted_publishing/github_configs', token, {'github_config': {'crate': name, **PUBLISHER}}))
    else:
        print('UNKNOWN  Trusted Publishing configuration (listing it needs a token)')
        missing.append('Trusted Publishing configuration')

    # 4. Last, because it stops this token from publishing the crate.
    step(crate['crate'].get('trustpub_only') is True, 'Trusted Publishing only',
         lambda: request('PATCH', f'/crates/{name}', token, {'crate': {'trustpub_only': True}}))

    if missing:
        sys.exit(1)
    if not args.check:
        print(f'\n{name} is ready for publish.yml. In the repository, add it to the publication order in\n'
              '.github/workflows/publish.yml (after the crates it depends on) and to the lists in\n'
              '.github/workflows/ci.yml, scripts/check-crate-boundaries.mjs and licenses/deny.toml.')


if __name__ == '__main__':
    try:
        main()
    except ApiError as error:
        sys.exit(str(error))
