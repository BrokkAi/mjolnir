#!/usr/bin/env python3
"""Builds the R2 review-arms instance files (Wizard-of-Oz site_context arm).

  setup.py render --variant J|O --out DIR     write config.toml with placeholders (no side effects elsewhere)
  setup.py apply  --variant J|O --target-file F --known-hosts-file F --key-file F [--r2 DIR]
        creates R2/instance/{config,data,carrier-dsh,dsh-review,codex-homes}, config.toml, secrets.toml,
        and (variant O) the codex homes. Does not start a daemon, create the workspace or log in.
        Variant J: homes stay /home/jonathan/.codexN; the overlay is NOT written there (Jonathan does it).
        Variant O: homes under R2/instance/codex-homes/codexN; run `mjx login --profile codexN` after.
Reads /home/jonathan/.codexN only to copy config.toml and AGENTS.md; never writes there.
"""
import argparse, base64, pathlib, re, shutil, sys

HERE = pathlib.Path(__file__).resolve().parent
BASE = HERE.parent / "instance-config.toml"
OLD_ROOT = "/mnt/optane/review-arms-20261113"
PROFILES = ["codex", "codex2", "codex3", "codex4"]
LOGIN = pathlib.Path.home()


def render(variant, r2, target, known_hosts):
    text = BASE.read_text().replace(OLD_ROOT, r2)
    # R's daemon may run at the same time; the phone listener needs its own port.
    text = text.replace('bind = "127.0.0.1:38517"', 'bind = "127.0.0.1:38518"')
    for name in PROFILES:  # drop the stock Terra/Sol profiles; they are re-added below
        text = re.sub(rf'\[profiles\.{name}\]\nkind = "codex"\nhome = "[^"]*"\n\n', "", text)
    out = []
    for name in PROFILES:
        home = f"{LOGIN}/.{name}" if variant == "J" else f"{r2}/instance/codex-homes/{name}"
        extra = "load_balancing_may_consume_credits = true\n" if name == "codex" else ""
        out.append(f'''[profiles.{name}]
kind = "codex"
home = "{home}"
codex_profile = "review_arms_oz"
{extra}
[profiles.{name}.environment]
SITE_CONTEXT_SSH_KEY_B64 = {{ from_secret = "SITE_CONTEXT_SSH_KEY_B64" }}
SITE_CONTEXT_SSH_TARGET = "{target}"
SITE_CONTEXT_SSH_KNOWN_HOSTS = "{known_hosts}"

''')
    marker = "[profiles.carrier]"
    assert marker in text
    return text.replace(marker, "".join(out) + marker, 1)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("cmd", choices=["render", "apply"])
    p.add_argument("--variant", required=True, choices=["J", "O"])
    p.add_argument("--out")
    p.add_argument("--r2", default="/mnt/optane/review-arms-oz")
    p.add_argument("--target-file")
    p.add_argument("--known-hosts-file")
    p.add_argument("--key-file")
    a = p.parse_args()
    if a.cmd == "render":
        out = pathlib.Path(a.out)
        out.mkdir(parents=True, exist_ok=True)
        (out / "config.toml").write_text(render(a.variant, a.r2, "<user@host from the EC2 host child>",
                                                "<one known_hosts line from the EC2 host child>"))
        return
    target = pathlib.Path(a.target_file).read_text().strip()
    known_hosts = pathlib.Path(a.known_hosts_file).read_text().strip()
    assert "\n" not in target and "\n" not in known_hosts and '"' not in target + known_hosts
    key = pathlib.Path(a.key_file).read_bytes()
    r2 = pathlib.Path(a.r2)
    inst = r2 / "instance"
    for d in ["config", "data", "carrier-dsh", "dsh-review"]:
        (inst / d).mkdir(parents=True, exist_ok=True)
    shutil.copy(HERE.parent / "carrier-AGENTS.md", inst / "carrier-dsh/AGENTS.md")
    (inst / "config/config.toml").write_text(render(a.variant, str(r2), target, known_hosts))
    secrets = [l for l in (LOGIN / ".config/mjolnir/secrets.toml").read_text().splitlines()
               if re.match(r"(DEEPSEEK_API_KEY|ZAI_API_KEY)\s*=", l)]
    secrets.append(f'SITE_CONTEXT_SSH_KEY_B64 = "{base64.b64encode(key).decode()}"')
    sp = inst / "config/secrets.toml"
    sp.touch(mode=0o600)
    sp.chmod(0o600)
    sp.write_text("\n".join(secrets) + "\n")
    if a.variant == "O":
        for name in PROFILES:
            home = inst / "codex-homes" / name
            home.mkdir(parents=True, exist_ok=True, mode=0o700)
            src = LOGIN / f".{name}"
            for f in ["config.toml", "AGENTS.md"]:
                if (src / f).exists():
                    shutil.copy(src / f, home / f)
            shutil.copy(HERE / "review_arms_oz.config.toml", home / "review_arms_oz.config.toml")
    print("done; next:", "mjx doctor, workspaces create" + ("; mjx login --profile codexN x4" if a.variant == "O" else ""))


main()
