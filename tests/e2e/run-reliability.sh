#!/usr/bin/env bash
set -euo pipefail

scenario=
seed=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --scenario)
            [[ $# -ge 2 ]] || { echo "--scenario requires a value" >&2; exit 2; }
            scenario=$2
            shift 2
            ;;
        --seed)
            [[ $# -ge 2 ]] || { echo "--seed requires a value" >&2; exit 2; }
            seed=$2
            shift 2
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "unknown option: $1" >&2
            exit 2
            ;;
        *)
            break
            ;;
    esac
done

if [[ -z $scenario || -z $seed || $# -ne 1 ]]; then
    echo "usage: $0 --scenario {multi-client-happy-path|active-stop} --seed NUMBER /path/to/hel" >&2
    exit 2
fi
if [[ $scenario != multi-client-happy-path && $scenario != active-stop ]]; then
    echo "unknown reliability scenario: $scenario" >&2
    exit 2
fi
if [[ ! $seed =~ ^[0-9]+$ ]]; then
    echo "seed must be an unsigned integer: $seed" >&2
    exit 2
fi
hel_binary=$1
if [[ ! -x $hel_binary ]]; then
    echo "Hel binary is not executable: $hel_binary" >&2
    exit 2
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
exec python3 "$script_dir/reliability_lab.py" \
    --scenario "$scenario" \
    --seed "$seed" \
    --hel "$hel_binary"
