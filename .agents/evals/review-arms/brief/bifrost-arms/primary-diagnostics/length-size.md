# Brief length and diff size diagnostic

Frozen17relationdiffs; exploratory, no newmodelcalls. Character/4 is only an approximate proxy, not a token count.

| Diff | Changed lines | Brief chars | Words | Items /25 | A caught /3 | BT caught /3 | Paired pp |
|---|---:|---:|---:|---:|---:|---:|---:|
| python-filescope-enclosing-tie | 977 | 5579 | 455 | 16 | 1 | 0 | -33.3 |
| cpp-template-inherited-base-signature | 130 | 5389 | 469 | 14 | 0 | 0 | +0.0 |
| incompatible-semantic-bundle-late-gate | 2936 | 6250 | 521 | 17 | 0 | 0 | +0.0 |
| retained-fill-java-memo-loses-gap | 109 | 5766 | 421 | 15 | 1 | 0 | -33.3 |
| kotlin-source-constructor-steals-class-header-comment | 675 | 5719 | 446 | 15 | 0 | 0 | +0.0 |
| mcp-default-advisory-reported-clean | 688 | 5403 | 442 | 17 | 0 | 0 | +0.0 |
| go-receiver-poisons-package-result-contract-discovery | 409 | 6210 | 484 | 13 | 0 | 0 | +0.0 |
| python-module-binding-representative-after-event | 182 | 5876 | 445 | 13 | 2 | 1 | -33.3 |
| jvm-protected-unindexed-hierarchy-denied | 837 | 5538 | 410 | 15 | 0 | 0 | +0.0 |
| python-module-mutation-repeated-scan-budget | 623 | 5808 | 458 | 17 | 0 | 0 | +0.0 |
| dependency-failure-drops-independent-model-evidence | 2220 | 6099 | 506 | 16 | 0 | 0 | +0.0 |
| php-indexed-write-reenters-current-rhs | 428 | 6262 | 495 | 15 | 2 | 0 | -66.7 |
| kotlin-value-class-this-loses-underlying-projection | 2915 | 4652 | 389 | 15 | 0 | 0 | +0.0 |
| go-nilness-ignores-modeled-nonreturn | 2032 | 4952 | 405 | 17 | 0 | 0 | +0.0 |
| java-static-getter-drops-incomplete-route | 817 | 5991 | 457 | 13 | 0 | 0 | +0.0 |
| unproven-override-family-certifies-empty | 360 | 6059 | 460 | 13 | 3 | 3 | +0.0 |
| js-synthetic-owner-leaks-file-summary | 144 | 4959 | 448 | 14 | 0 | 0 | +0.0 |

{
  "scope": "registered17relationdiffs; artifact-only exploratorydiagnostic, no newmodelcalls/no primarybar changes",
  "n": 17,
  "negative_diffs": 4,
  "zero_diffs": 13,
  "positive_diffs": 0,
  "all30_bug_diff_median": 827.0,
  "primary17_diff_median": 675,
  "brief_characters_median": 5766,
  "brief_items_median": 15,
  "brief_characters_range": [
    4652,
    6262
  ],
  "brief_items_range": [
    13,
    17
  ],
  "exact25item_briefs": 0,
  "negative_diff_size_median": 305.0,
  "zero_diff_size_median": 688,
  "token_limit_note": "25items and about1500tokens are registered; script uses7000characters with slack. Characters/4 is only a rough proxy, NOT exact tokenization or whole-writer usage.",
  "interpretation_limit": "Descriptive17case table only; no causal size/budget inference or posthoccase selection."
}
