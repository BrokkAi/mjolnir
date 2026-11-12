Frozen aliases600 reference assets (#1505). Python Unicode 16.0.0.

RUST-PORT.md: d12cf4fdc6de59b9f5def989dcfc70e308dcd241bd4640afc170a415f96e44a0
history2.py: 50a04132b03af5834f14e370eab77a8bfa0c637bc95b1380c00a1f57cf2e1f99
common.py: 3f1ab2be8f102476de84fff881eecdcbb3e9effb50aa5efd96acd6abde5d624d
jev_shape.py: 60dc8fa6600bdaf0d48e533b293040afa68aed89736c9461e1c6893e34d50ce4
ettin-tokenizer.json: 28c5e078e4c52aa37cf0e6de1a212878f3dbd58dd1c70466298efe0b6b86db35
out/python-unicode-ranges.json: babeb3d0c214138040d7c77e0c00b3c2360bb25361893d99b41f737e1d3c7daf
out/rust-port-fixtures.jsonl.gz: 1a8960f99d031753bf06d51ab1b304b4e49fc0f97229bd8e68f0c08b1ce81734

reference.jsonl is the decompressed 16-row frozen fixture corpus, with an
expected_evidence_json field added by Python json.dumps using the reference
session_order and each existing expected_history. No Rust output generated
expected values. unicode-adversarial.json was generated with the same frozen
history2.py/jev_shape.py on decimal digits, superscripts, combining marks,
connectors, asymmetric underscores and filenames adjacent to Unicode words.
It checks Python identifier insertion order and the complete lexical output.

tool-numbers.json contains the frozen dossier_reference.py render_row output
for a persisted tool-call shape with arbitrary rawInput/rawOutput numbers:
notation thresholds, signed zero, subnormal/maximal finite values and 200
seeded float values. Expected bytes were emitted by Python, including its
shortest float representation and exponent formatting.

The expected_item_segment and expected_session_segment fields are Python's
compact UTF-8 serialization of the same final evidence, matching common.py's
item-session tokenizer inputs. They cover student_segments on all 16 rows.
