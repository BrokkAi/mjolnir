Verdicts counted: confirmed; severities: high, low, medium. Diffs: 70 (skipped for missing sessions: 0).

| Reviewers | Median latency (s) | Findings (total) | Per diff | Est. cost per diff ($) | Frontier | Cost frontier | Bootstrap frontier share |
|---|---:|---:|---:|---:|:---:|:---:|---:|
| Terra/high | 43.1 | 100.17 | 1.431 | 0.044 | yes | yes | 1.0 |
| Sol/medium | 46.2 | 50.0 | 0.714 | 0.1 |  |  | 0.06 |
| 2× Terra/high | 47.3 | 126.33 | 1.805 | 0.087 | yes | yes | 1.0 |
| 3× Terra/high | 51.7 | 141.0 | 2.014 | 0.131 | yes |  | 1.0 |
| Sol/medium + Terra/high | 52.1 | 113.12 | 1.616 | 0.143 |  |  | 0.003 |
| 2× Sol/medium | 53.0 | 66.0 | 0.943 | 0.2 |  |  | 0.0 |
| Sol/medium + 2× Terra/high | 54.3 | 135.5 | 1.936 | 0.187 |  |  | 0.04 |
| 2× Sol/medium + Terra/high | 55.6 | 120.75 | 1.725 | 0.243 |  |  | 0.0 |
| Gemini/medium | 75.0 | 32.0 | 0.457 | None |  |  | 0.0 |
| Gemini/medium + Sol/medium | 76.2 | 66.0 | 0.943 | None |  |  | 0.0 |
| Gemini/medium + Terra/high | 77.2 | 112.33 | 1.605 | None |  |  | 0.0 |
| Gemini/medium + 2× Sol/medium | 77.5 | 79.0 | 1.129 | None |  |  | 0.0 |
| Gemini/medium + Sol/medium + Terra/high | 77.8 | 123.79 | 1.768 | None |  |  | 0.0 |
| Gemini/medium + 2× Terra/high | 78.5 | 135.5 | 1.936 | None |  |  | 0.047 |
| Terra/xhigh | 83.8 | 183.5 | 2.621 | 0.093 | yes | yes | 0.387 |
| Sol/medium + Terra/xhigh | 84.0 | 190.0 | 2.714 | 0.193 | yes |  | 0.51 |
| 2× Sol/medium + Terra/xhigh | 84.1 | 192.0 | 2.743 | 0.293 | yes |  | 0.773 |
| Terra/high + Terra/xhigh | 84.8 | 196.96 | 2.814 | 0.137 | yes | yes | 0.493 |
| Sol/medium + Terra/high + Terra/xhigh | 85.0 | 201.65 | 2.881 | 0.236 | yes |  | 0.91 |
| 2× Terra/high + Terra/xhigh | 85.9 | 204.08 | 2.915 | 0.18 | yes | yes | 0.907 |
| Sol/high | 90.1 | 84.33 | 1.205 | 0.168 |  |  | 0.0 |
| Sol/high + Sol/medium | 90.1 | 91.67 | 1.31 | 0.267 |  |  | 0.0 |
| Sol/high + 2× Sol/medium | 90.1 | 97.33 | 1.39 | 0.367 |  |  | 0.0 |
| Gemini/medium + Sol/medium + Terra/xhigh | 91.6 | 198.75 | 2.839 | None |  |  | 0.04 |
| Gemini/medium + Terra/xhigh | 91.7 | 193.5 | 2.764 | None |  |  | 0.0 |
| Sol/high + Terra/high | 91.9 | 136.53 | 1.95 | 0.211 |  |  | 0.0 |
| Sol/high + Sol/medium + Terra/high | 92.0 | 139.67 | 1.995 | 0.311 |  |  | 0.007 |
| Gemini/medium + Terra/high + Terra/xhigh | 92.6 | 204.62 | 2.923 | None | yes |  | 0.343 |
| 2× Terra/xhigh | 92.8 | 225.0 | 3.214 | 0.186 |  | yes | 0.0 |
| Sol/medium + 2× Terra/xhigh | 92.8 | 228.5 | 3.264 | 0.286 | yes |  | 0.873 |
| Terra/high + 2× Terra/xhigh | 93.6 | 232.25 | 3.318 | 0.23 | yes | yes | 0.93 |
| Sol/high + 2× Terra/high | 93.9 | 155.61 | 2.223 | 0.255 |  |  | 0.003 |
| Gemini/medium + Sol/high | 95.0 | 99.67 | 1.424 | None |  |  | 0.0 |
| Gemini/medium + Sol/high + Sol/medium | 95.0 | 105.33 | 1.505 | None |  |  | 0.0 |
| Gemini/medium + Sol/high + Terra/high | 96.2 | 146.53 | 2.093 | None |  |  | 0.0 |
| Gemini/medium + 2× Terra/xhigh | 97.2 | 232.0 | 3.314 | None |  |  | 0.42 |
| Sol/high + Terra/xhigh | 99.4 | 198.83 | 2.84 | 0.26 |  |  | 0.0 |
| Sol/high + Sol/medium + Terra/xhigh | 99.4 | 200.75 | 2.868 | 0.36 |  |  | 0.0 |
| 2× Sol/high | 100.0 | 107.33 | 1.533 | 0.335 |  |  | 0.0 |
| 2× Sol/high + Sol/medium | 100.0 | 110.33 | 1.576 | 0.435 |  |  | 0.0 |
| Sol/high + Terra/high + Terra/xhigh | 100.2 | 210.31 | 3.004 | 0.304 |  |  | 0.003 |
| 2× Sol/high + Terra/high | 101.1 | 150.42 | 2.149 | 0.379 |  |  | 0.0 |
| Gemini/medium + 2× Sol/high | 101.5 | 121.33 | 1.733 | None |  |  | 0.0 |
| Gemini/medium + Sol/high + Terra/xhigh | 101.5 | 207.83 | 2.969 | None |  |  | 0.0 |
| Sol/high + 2× Terra/xhigh | 102.6 | 235.0 | 3.357 | 0.353 | yes | yes | 0.687 |
| 3× Sol/high | 103.6 | 120.0 | 1.714 | 0.503 |  |  | 0.0 |
| 2× Sol/high + Terra/xhigh | 103.8 | 206.33 | 2.948 | 0.428 |  |  | 0.0 |
| Greptile/base | 146.8 | 124.0 | 1.771 | 1.0 |  |  | 0.0 |
| Greptile/base + Sol/medium | 146.8 | 134.5 | 1.921 | 1.1 |  |  | 0.0 |
| Greptile/base + 2× Sol/medium | 146.8 | 140.0 | 2.0 | 1.2 |  |  | 0.0 |
| Gemini/medium + Greptile/base | 147.5 | 136.0 | 1.943 | None |  |  | 0.0 |
| Gemini/medium + Greptile/base + Sol/medium | 147.5 | 145.0 | 2.071 | None |  |  | 0.0 |
| Greptile/base + Sol/high | 148.2 | 151.0 | 2.157 | 1.168 |  |  | 0.0 |
| Greptile/base + Sol/high + Sol/medium | 148.2 | 153.33 | 2.19 | 1.267 |  |  | 0.0 |
| Greptile/base + Terra/high | 148.8 | 169.08 | 2.415 | 1.044 |  |  | 0.0 |
| Gemini/medium + Greptile/base + Sol/high | 148.9 | 161.33 | 2.305 | None |  |  | 0.0 |
| Greptile/base + 2× Sol/high | 148.9 | 162.0 | 2.314 | 1.335 |  |  | 0.0 |
| Greptile/base + Sol/medium + Terra/high | 148.9 | 171.88 | 2.455 | 1.143 |  |  | 0.0 |
| Gemini/medium + Greptile/base + Terra/high | 149.4 | 176.92 | 2.527 | None |  |  | 0.0 |
| Greptile/base + Sol/medium + Terra/xhigh | 149.7 | 228.0 | 3.257 | 1.193 |  |  | 0.05 |
| Greptile/base + Terra/xhigh | 149.8 | 226.5 | 3.236 | 1.093 |  |  | 0.04 |
| Gemini/medium + Greptile/base + Terra/xhigh | 150.0 | 234.0 | 3.343 | None |  |  | 0.283 |
| Greptile/base + Sol/high + Terra/high | 150.0 | 183.94 | 2.628 | 1.211 |  |  | 0.0 |
| Greptile/base + Sol/high + Terra/xhigh | 150.5 | 234.5 | 3.35 | 1.26 |  |  | 0.163 |
| Greptile/base + 2× Terra/high | 150.5 | 186.5 | 2.664 | 1.087 |  |  | 0.0 |
| Greptile/base + Terra/high + Terra/xhigh | 150.6 | 234.58 | 3.351 | 1.137 |  |  | 0.143 |
| Greptile/base + 2× Terra/xhigh | 151.2 | 258.0 | 3.686 | 1.186 | yes | yes | 1.0 |
| Gemini/high | 199.5 | 80.0 | 1.143 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium | 199.5 | 83.0 | 1.186 | None |  |  | 0.0 |
| Gemini/high + Greptile/base | 199.5 | 162.0 | 2.314 | None |  |  | 0.0 |
| Gemini/high + Sol/high | 199.5 | 126.33 | 1.805 | None |  |  | 0.0 |
| Gemini/high + Sol/medium | 199.5 | 103.5 | 1.479 | None |  |  | 0.0 |
| Gemini/high + Terra/xhigh | 199.5 | 211.0 | 3.014 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Greptile/base | 199.5 | 165.0 | 2.357 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Sol/high | 199.5 | 129.33 | 1.848 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Sol/medium | 199.5 | 106.0 | 1.514 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Terra/xhigh | 199.5 | 213.0 | 3.043 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/high | 199.5 | 179.67 | 2.567 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/medium | 199.5 | 169.5 | 2.421 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Terra/xhigh | 199.5 | 247.5 | 3.536 | None |  |  | 0.03 |
| Gemini/high + 2× Sol/high | 199.5 | 143.0 | 2.043 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Sol/medium | 199.5 | 131.83 | 1.883 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Terra/xhigh | 199.5 | 222.67 | 3.181 | None |  |  | 0.0 |
| Gemini/high + 2× Sol/medium | 199.5 | 114.0 | 1.629 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Terra/xhigh | 199.5 | 215.5 | 3.079 | None |  |  | 0.0 |
| Gemini/high + 2× Terra/xhigh | 199.5 | 247.0 | 3.529 | None |  |  | 0.043 |
| Gemini/high + Greptile/base + Terra/high | 201.8 | 199.83 | 2.855 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Terra/high | 201.8 | 153.54 | 2.193 | None |  |  | 0.0 |
| Gemini/high + Terra/high + Terra/xhigh | 201.8 | 221.96 | 3.171 | None |  |  | 0.0 |
| Gemini/high + Terra/high | 201.9 | 145.92 | 2.085 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Terra/high | 201.9 | 147.58 | 2.108 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Terra/high | 201.9 | 168.81 | 2.412 | None |  |  | 0.0 |
| Gemini/high + 2× Terra/high | 204.2 | 164.83 | 2.355 | None |  |  | 0.0 |
| Sol/high + Sol/xhigh | 211.4 | 140.67 | 2.01 | 0.497 |  |  | 0.0 |
| Sol/xhigh | 211.5 | 131.0 | 1.871 | 0.33 |  |  | 0.0 |
| Sol/xhigh + Terra/xhigh | 211.5 | 223.5 | 3.193 | 0.423 |  |  | 0.0 |
| 2× Sol/medium + Sol/xhigh | 211.5 | 138.0 | 1.971 | 0.529 |  |  | 0.0 |
| Gemini/medium + Sol/xhigh | 211.6 | 143.0 | 2.043 | None |  |  | 0.0 |
| Gemini/medium + Sol/medium + Sol/xhigh | 211.6 | 145.5 | 2.079 | None |  |  | 0.0 |
| Gemini/medium + Sol/xhigh + Terra/xhigh | 211.6 | 230.75 | 3.296 | None |  |  | 0.0 |
| 2× Sol/high + Sol/xhigh | 211.6 | 147.83 | 2.112 | 0.665 |  |  | 0.0 |
| Sol/high + Sol/medium + Sol/xhigh | 211.6 | 143.08 | 2.044 | 0.597 |  |  | 0.0 |
| Sol/medium + Sol/xhigh | 211.7 | 134.75 | 1.925 | 0.43 |  |  | 0.0 |
| Gemini/medium + Sol/high + Sol/xhigh | 211.7 | 151.67 | 2.167 | None |  |  | 0.0 |
| Sol/medium + Sol/xhigh + Terra/xhigh | 211.7 | 225.0 | 3.214 | 0.523 |  |  | 0.0 |
| Sol/xhigh + 2× Terra/xhigh | 211.7 | 256.5 | 3.664 | 0.516 |  | yes | 0.353 |
| Sol/high + Sol/xhigh + Terra/xhigh | 211.8 | 226.67 | 3.238 | 0.59 |  |  | 0.0 |
| Sol/xhigh + Terra/high | 213.2 | 176.04 | 2.515 | 0.373 |  |  | 0.0 |
| Gemini/medium + Sol/xhigh + Terra/high | 213.2 | 183.04 | 2.615 | None |  |  | 0.0 |
| Sol/high + Sol/xhigh + Terra/high | 213.3 | 181.51 | 2.593 | 0.541 |  |  | 0.0 |
| Sol/medium + Sol/xhigh + Terra/high | 213.3 | 177.75 | 2.539 | 0.473 |  |  | 0.0 |
| Sol/xhigh + Terra/high + Terra/xhigh | 213.4 | 233.54 | 3.336 | 0.466 |  |  | 0.0 |
| Sol/xhigh + 2× Terra/high | 214.7 | 193.17 | 2.76 | 0.417 |  |  | 0.0 |
| Greptile/base + Sol/xhigh | 216.3 | 178.0 | 2.543 | 1.33 |  |  | 0.0 |
| Gemini/medium + Greptile/base + Sol/xhigh | 216.3 | 186.5 | 2.664 | None |  |  | 0.0 |
| Greptile/base + Sol/high + Sol/xhigh | 216.3 | 184.33 | 2.633 | 1.497 |  |  | 0.0 |
| Greptile/base + Sol/xhigh + Terra/xhigh | 216.3 | 251.5 | 3.593 | 1.423 |  |  | 0.083 |
| Greptile/base + Sol/medium + Sol/xhigh | 216.5 | 180.0 | 2.571 | 1.43 |  |  | 0.0 |
| Greptile/base + Sol/xhigh + Terra/high | 217.5 | 209.17 | 2.988 | 1.373 |  |  | 0.0 |
| 2× Sol/xhigh | 226.0 | 162.0 | 2.314 | 0.659 |  |  | 0.0 |
| Gemini/medium + 2× Sol/xhigh | 226.0 | 172.0 | 2.457 | None |  |  | 0.0 |
| Sol/high + 2× Sol/xhigh | 226.0 | 167.0 | 2.386 | 0.827 |  |  | 0.0 |
| Sol/medium + 2× Sol/xhigh | 226.0 | 163.0 | 2.329 | 0.759 |  |  | 0.0 |
| 2× Sol/xhigh + Terra/xhigh | 226.0 | 242.5 | 3.464 | 0.752 |  |  | 0.003 |
| Greptile/base + 2× Sol/xhigh | 227.1 | 200.0 | 2.857 | 1.659 |  |  | 0.0 |
| 2× Sol/xhigh + Terra/high | 227.8 | 201.33 | 2.876 | 0.703 |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/xhigh | 238.0 | 201.0 | 2.871 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Sol/xhigh | 238.0 | 168.67 | 2.41 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Sol/xhigh | 238.0 | 164.75 | 2.354 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Sol/xhigh | 238.1 | 165.0 | 2.357 | None |  |  | 0.0 |
| Gemini/high + Sol/xhigh | 238.2 | 162.0 | 2.314 | None |  |  | 0.0 |
| Gemini/high + Sol/xhigh + Terra/xhigh | 238.2 | 242.0 | 3.457 | None |  |  | 0.0 |
| Gemini/high + Sol/xhigh + Terra/high | 238.8 | 198.46 | 2.835 | None |  |  | 0.0 |
| Gemini/high + 2× Sol/xhigh | 245.9 | 187.0 | 2.671 | None |  |  | 0.0 |
| Greptile/plus | 265.9 | 190.0 | 2.714 | 3.0 |  |  | 0.0 |
| Gemini/medium + Greptile/plus | 265.9 | 198.0 | 2.829 | None |  |  | 0.0 |
| Greptile/base + Greptile/plus | 265.9 | 202.0 | 2.886 | 4.0 |  |  | 0.0 |
| Greptile/plus + Sol/high | 265.9 | 202.0 | 2.886 | 3.168 |  |  | 0.0 |
| Greptile/plus + Sol/medium | 265.9 | 192.5 | 2.75 | 3.1 |  |  | 0.0 |
| Greptile/plus + Terra/high | 265.9 | 219.58 | 3.137 | 3.044 |  |  | 0.0 |
| Greptile/plus + Terra/xhigh | 265.9 | 266.0 | 3.8 | 3.093 |  | yes | 0.0 |
| Gemini/medium + Greptile/base + Greptile/plus | 265.9 | 210.0 | 3.0 | None |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Sol/high | 265.9 | 209.67 | 2.995 | None |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Sol/medium | 265.9 | 200.0 | 2.857 | None |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Terra/high | 265.9 | 225.08 | 3.215 | None |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Terra/xhigh | 265.9 | 271.5 | 3.879 | None |  |  | 0.0 |
| Greptile/base + Greptile/plus + Sol/high | 265.9 | 211.0 | 3.014 | 4.168 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Sol/medium | 265.9 | 203.5 | 2.907 | 4.1 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Terra/high | 265.9 | 228.0 | 3.257 | 4.044 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Terra/xhigh | 265.9 | 273.0 | 3.9 | 4.093 |  |  | 0.0 |
| Greptile/plus + 2× Sol/high | 265.9 | 207.67 | 2.967 | 3.335 |  |  | 0.0 |
| Greptile/plus + Sol/high + Sol/medium | 265.9 | 202.83 | 2.898 | 3.267 |  |  | 0.0 |
| Greptile/plus + Sol/high + Terra/high | 265.9 | 228.58 | 3.265 | 3.211 |  |  | 0.0 |
| Greptile/plus + Sol/high + Terra/xhigh | 265.9 | 271.17 | 3.874 | 3.26 |  |  | 0.0 |
| Greptile/plus + 2× Sol/medium | 265.9 | 194.0 | 2.771 | 3.2 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Terra/high | 265.9 | 221.25 | 3.161 | 3.143 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Terra/xhigh | 265.9 | 267.5 | 3.821 | 3.193 |  |  | 0.0 |
| Greptile/plus + 2× Terra/high | 265.9 | 231.33 | 3.305 | 3.087 |  |  | 0.0 |
| Greptile/plus + Terra/high + Terra/xhigh | 265.9 | 272.42 | 3.892 | 3.137 |  | yes | 0.0 |
| Greptile/plus + 2× Terra/xhigh | 265.9 | 293.0 | 4.186 | 3.186 | yes | yes | 1.0 |
| Gemini/high + Greptile/plus | 274.9 | 214.0 | 3.057 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Greptile/plus | 274.9 | 217.0 | 3.1 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Greptile/plus | 274.9 | 224.0 | 3.2 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/high | 274.9 | 223.0 | 3.186 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/medium | 274.9 | 216.0 | 3.086 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Terra/xhigh | 274.9 | 280.0 | 4.0 | None |  |  | 0.01 |
| Gemini/high + Greptile/plus + Terra/high | 275.7 | 240.08 | 3.43 | None |  |  | 0.0 |
| Greptile/base + Greptile/plus + Sol/xhigh | 286.3 | 229.5 | 3.279 | 4.33 |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Sol/xhigh | 286.4 | 227.5 | 3.25 | None |  |  | 0.0 |
| Greptile/plus + Sol/xhigh | 286.6 | 221.0 | 3.157 | 3.33 |  |  | 0.0 |
| Greptile/plus + Sol/high + Sol/xhigh | 286.6 | 224.67 | 3.21 | 3.497 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Sol/xhigh | 286.6 | 221.75 | 3.168 | 3.43 |  |  | 0.0 |
| Greptile/plus + Sol/xhigh + Terra/xhigh | 286.7 | 283.5 | 4.05 | 3.423 |  |  | 0.033 |
| Greptile/plus + Sol/xhigh + Terra/high | 288.3 | 247.12 | 3.53 | 3.373 |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/xhigh | 288.5 | 238.5 | 3.407 | None |  |  | 0.0 |
| Ember/xhigh | 292.4 | 33.0 | 0.471 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium | 292.4 | 52.0 | 0.743 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base | 292.4 | 134.0 | 1.914 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high | 292.4 | 98.0 | 1.4 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium | 292.4 | 66.0 | 0.943 | None |  |  | 0.0 |
| Ember/xhigh + Terra/xhigh | 292.4 | 194.0 | 2.771 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Greptile/base | 292.4 | 144.0 | 2.057 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Sol/high | 292.4 | 111.0 | 1.586 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Sol/medium | 292.4 | 78.5 | 1.121 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Terra/xhigh | 292.4 | 202.0 | 2.886 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/high | 292.4 | 157.67 | 2.252 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/medium | 292.4 | 143.0 | 2.043 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Terra/xhigh | 292.4 | 231.0 | 3.3 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/high | 292.4 | 118.0 | 1.686 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Sol/medium | 292.4 | 103.5 | 1.479 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Terra/xhigh | 292.4 | 207.5 | 2.964 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/medium | 292.4 | 78.0 | 1.114 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Terra/xhigh | 292.4 | 199.0 | 2.843 | None |  |  | 0.0 |
| Ember/xhigh + 2× Terra/xhigh | 292.4 | 233.0 | 3.329 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Terra/high | 296.4 | 123.71 | 1.767 | None |  |  | 0.0 |
| Ember/xhigh + Terra/high + Terra/xhigh | 296.4 | 205.29 | 2.933 | None |  |  | 0.0 |
| Ember/xhigh + Terra/high | 296.5 | 113.75 | 1.625 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Terra/high | 296.6 | 123.25 | 1.761 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Terra/high | 296.6 | 175.67 | 2.51 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Terra/high | 296.6 | 145.92 | 2.085 | None |  |  | 0.0 |
| Greptile/plus + 2× Sol/xhigh | 297.4 | 238.0 | 3.4 | 3.659 |  |  | 0.0 |
| Ember/xhigh + 2× Terra/high | 300.7 | 137.0 | 1.957 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high | 304.5 | 94.0 | 1.343 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Gemini/medium | 304.5 | 97.0 | 1.386 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Greptile/base | 304.5 | 168.0 | 2.4 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/high | 304.5 | 136.33 | 1.948 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/medium | 304.5 | 113.5 | 1.621 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Terra/xhigh | 304.5 | 219.0 | 3.129 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Terra/high | 305.7 | 154.58 | 2.208 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh | 319.5 | 141.0 | 2.014 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/xhigh | 319.5 | 184.5 | 2.636 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/xhigh | 319.6 | 168.0 | 2.4 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Sol/xhigh | 319.6 | 150.0 | 2.143 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Sol/xhigh | 319.6 | 144.0 | 2.057 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Sol/xhigh | 319.7 | 149.33 | 2.133 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh + Terra/xhigh | 319.7 | 229.75 | 3.282 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh + Terra/high | 321.8 | 183.29 | 2.618 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/xhigh | 333.3 | 169.0 | 2.414 | None |  |  | 0.0 |
| Kimi/high | 340.2 | 72.0 | 1.029 | None |  |  | 0.0 |
| Gemini/high + Kimi/high | 340.2 | 116.0 | 1.657 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high | 340.2 | 88.0 | 1.257 | None |  |  | 0.0 |
| Greptile/base + Kimi/high | 340.2 | 153.0 | 2.186 | None |  |  | 0.0 |
| Kimi/high + Sol/high | 340.2 | 115.67 | 1.652 | None |  |  | 0.0 |
| Kimi/high + Sol/medium | 340.2 | 95.0 | 1.357 | None |  |  | 0.0 |
| Kimi/high + Terra/xhigh | 340.2 | 204.0 | 2.914 | None |  |  | 0.0 |
| Gemini/high + Gemini/medium + Kimi/high | 340.2 | 119.0 | 1.7 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Kimi/high | 340.2 | 180.0 | 2.571 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/high | 340.2 | 145.33 | 2.076 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/medium | 340.2 | 130.0 | 1.857 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Terra/xhigh | 340.2 | 225.0 | 3.214 | None |  |  | 0.0 |
| Gemini/medium + Greptile/base + Kimi/high | 340.2 | 162.0 | 2.314 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high + Sol/high | 340.2 | 126.33 | 1.805 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high + Sol/medium | 340.2 | 105.5 | 1.507 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high + Terra/xhigh | 340.2 | 209.5 | 2.993 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/high | 340.2 | 169.67 | 2.424 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/medium | 340.2 | 159.5 | 2.279 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Terra/xhigh | 340.2 | 241.0 | 3.443 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/high | 340.2 | 132.67 | 1.895 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Sol/medium | 340.2 | 121.5 | 1.736 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Terra/xhigh | 340.2 | 214.83 | 3.069 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/medium | 340.2 | 105.0 | 1.5 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Terra/xhigh | 340.2 | 209.0 | 2.986 | None |  |  | 0.0 |
| Kimi/high + 2× Terra/xhigh | 340.2 | 242.0 | 3.457 | None |  |  | 0.0 |
| Kimi/high + Terra/high | 341.6 | 135.08 | 1.93 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Terra/high | 341.6 | 164.58 | 2.351 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high + Terra/high | 341.6 | 141.92 | 2.027 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Terra/high | 341.6 | 188.67 | 2.695 | None |  |  | 0.0 |
| Kimi/high + Terra/high + Terra/xhigh | 341.6 | 214.62 | 3.066 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Terra/high | 341.7 | 157.25 | 2.246 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Terra/high | 341.7 | 142.71 | 2.039 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus | 341.8 | 195.0 | 2.786 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Greptile/plus | 341.8 | 219.0 | 3.129 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Greptile/plus | 341.8 | 203.0 | 2.9 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Greptile/plus | 341.8 | 207.0 | 2.957 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/high | 341.8 | 207.0 | 2.957 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/medium | 341.8 | 197.5 | 2.821 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Terra/xhigh | 341.8 | 270.5 | 3.864 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh | 342.3 | 153.5 | 2.193 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/xhigh | 342.3 | 175.0 | 2.5 | None |  |  | 0.0 |
| Gemini/medium + Kimi/high + Sol/xhigh | 342.3 | 161.0 | 2.3 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/xhigh | 342.3 | 193.5 | 2.764 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Sol/xhigh | 342.3 | 161.67 | 2.31 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Sol/xhigh | 342.3 | 156.75 | 2.239 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh + Terra/xhigh | 342.3 | 235.75 | 3.368 | None |  |  | 0.0 |
| Kimi/high + 2× Terra/high | 343.0 | 154.33 | 2.205 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Terra/high | 343.2 | 224.58 | 3.208 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh + Terra/high | 343.7 | 191.46 | 2.735 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/xhigh | 344.4 | 182.0 | 2.6 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/xhigh | 346.2 | 225.5 | 3.221 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high | 352.1 | 209.0 | 2.986 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Kimi/high | 352.1 | 228.0 | 3.257 | None |  |  | 0.0 |
| Gemini/medium + Greptile/plus + Kimi/high | 352.1 | 216.0 | 3.086 | None |  |  | 0.0 |
| Greptile/base + Greptile/plus + Kimi/high | 352.1 | 219.0 | 3.129 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/high | 352.1 | 217.0 | 3.1 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/medium | 352.1 | 211.0 | 3.014 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Terra/xhigh | 352.1 | 277.0 | 3.957 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Terra/high | 354.3 | 233.58 | 3.337 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/xhigh | 355.4 | 234.0 | 3.343 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high | 386.7 | 84.0 | 1.2 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Kimi/high | 386.7 | 125.0 | 1.786 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/medium + Kimi/high | 386.7 | 97.0 | 1.386 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Kimi/high | 386.7 | 159.0 | 2.271 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/high | 386.7 | 123.67 | 1.767 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/medium | 386.7 | 103.0 | 1.471 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Terra/xhigh | 386.7 | 211.0 | 3.014 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Terra/high | 387.0 | 142.08 | 2.03 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/xhigh | 390.0 | 159.5 | 2.279 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Kimi/high | 393.4 | 214.0 | 3.057 | None |  |  | 0.0 |
