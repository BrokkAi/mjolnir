Verdicts counted: confirmed; severities: high, low, medium. Diffs: 70 (skipped for missing sessions: 0).

| Reviewers | Median latency (s) | Findings (total) | Per diff | Est. cost per diff ($) | Frontier | Cost frontier | Bootstrap frontier share |
|---|---:|---:|---:|---:|:---:|:---:|---:|
| Terra/high | 43.1 | 85.25 | 1.218 | 0.044 | yes | yes | 1.0 |
| Sol/medium | 46.3 | 48.5 | 0.693 | 0.1 |  |  | 0.083 |
| 2× Terra/high | 47.3 | 107.17 | 1.531 | 0.087 | yes | yes | 1.0 |
| 3× Terra/high | 51.7 | 118.0 | 1.686 | 0.131 | yes |  | 0.997 |
| Sol/medium + Terra/high | 52.2 | 98.71 | 1.41 | 0.143 |  |  | 0.013 |
| 2× Sol/medium | 53.0 | 63.0 | 0.9 | 0.2 |  |  | 0.0 |
| Sol/medium + 2× Terra/high | 54.3 | 116.33 | 1.662 | 0.187 |  |  | 0.263 |
| 2× Sol/medium + Terra/high | 55.6 | 106.5 | 1.521 | 0.243 |  |  | 0.01 |
| Terra/xhigh | 83.9 | 150.5 | 2.15 | 0.093 | yes | yes | 0.36 |
| Sol/medium + Terra/xhigh | 84.0 | 157.25 | 2.246 | 0.193 | yes |  | 0.503 |
| 2× Sol/medium + Terra/xhigh | 84.1 | 159.5 | 2.279 | 0.293 | yes |  | 0.82 |
| Terra/high + Terra/xhigh | 84.9 | 163.04 | 2.329 | 0.137 |  | yes | 0.503 |
| Sol/medium + Terra/high + Terra/xhigh | 84.9 | 167.81 | 2.397 | 0.236 | yes |  | 0.917 |
| 2× Terra/high + Terra/xhigh | 85.9 | 168.67 | 2.41 | 0.18 | yes | yes | 0.753 |
| Sol/high + Sol/medium | 90.0 | 86.67 | 1.238 | 0.267 |  |  | 0.0 |
| Sol/high | 90.1 | 80.0 | 1.143 | 0.168 |  |  | 0.0 |
| Sol/high + 2× Sol/medium | 90.1 | 91.67 | 1.31 | 0.367 |  |  | 0.0 |
| Sol/high + Terra/high | 92.0 | 120.89 | 1.727 | 0.211 |  |  | 0.007 |
| Sol/high + Sol/medium + Terra/high | 92.0 | 124.03 | 1.772 | 0.311 |  |  | 0.017 |
| 2× Terra/xhigh | 92.8 | 181.0 | 2.586 | 0.186 |  | yes | 0.013 |
| Sol/medium + 2× Terra/xhigh | 92.8 | 184.5 | 2.636 | 0.286 | yes |  | 0.89 |
| Terra/high + 2× Terra/xhigh | 93.6 | 188.25 | 2.689 | 0.23 | yes | yes | 0.94 |
| Sol/high + 2× Terra/high | 93.8 | 135.89 | 1.941 | 0.255 |  |  | 0.017 |
| Sol/high + Terra/xhigh | 99.4 | 165.67 | 2.367 | 0.26 |  |  | 0.0 |
| Sol/high + Sol/medium + Terra/xhigh | 99.4 | 167.58 | 2.394 | 0.36 |  |  | 0.007 |
| 2× Sol/high | 100.0 | 100.0 | 1.429 | 0.335 |  |  | 0.0 |
| 2× Sol/high + Sol/medium | 100.0 | 102.83 | 1.469 | 0.435 |  |  | 0.0 |
| Sol/high + Terra/high + Terra/xhigh | 100.2 | 175.94 | 2.513 | 0.304 |  |  | 0.013 |
| 2× Sol/high + Terra/high | 101.1 | 133.31 | 1.904 | 0.379 |  |  | 0.0 |
| Sol/high + 2× Terra/xhigh | 102.6 | 191.0 | 2.729 | 0.353 | yes | yes | 0.81 |
| 3× Sol/high | 103.6 | 111.0 | 1.586 | 0.503 |  |  | 0.0 |
| 2× Sol/high + Terra/xhigh | 103.8 | 172.67 | 2.467 | 0.428 |  |  | 0.0 |
| Greptile/base | 146.8 | 115.0 | 1.643 | 1.0 |  |  | 0.0 |
| Greptile/base + Sol/medium | 146.8 | 124.5 | 1.779 | 1.1 |  |  | 0.0 |
| Greptile/base + 2× Sol/medium | 146.8 | 129.0 | 1.843 | 1.2 |  |  | 0.0 |
| Greptile/base + Sol/high | 148.2 | 139.33 | 1.99 | 1.168 |  |  | 0.0 |
| Greptile/base + Sol/high + Sol/medium | 148.2 | 141.17 | 2.017 | 1.267 |  |  | 0.0 |
| Greptile/base + Terra/high | 148.8 | 149.58 | 2.137 | 1.044 |  |  | 0.0 |
| Greptile/base + 2× Sol/high | 148.9 | 148.33 | 2.119 | 1.335 |  |  | 0.0 |
| Greptile/base + Sol/medium + Terra/high | 148.9 | 152.38 | 2.177 | 1.143 |  |  | 0.0 |
| Greptile/base + Sol/medium + Terra/xhigh | 149.7 | 193.5 | 2.764 | 1.193 | yes |  | 0.407 |
| Greptile/base + Terra/xhigh | 149.8 | 192.0 | 2.743 | 1.093 |  |  | 0.163 |
| Greptile/base + Sol/high + Terra/high | 150.0 | 163.78 | 2.34 | 1.211 |  |  | 0.0 |
| Greptile/base + 2× Terra/high | 150.5 | 163.67 | 2.338 | 1.087 |  |  | 0.0 |
| Greptile/base + Sol/high + Terra/xhigh | 150.6 | 199.33 | 2.848 | 1.26 | yes |  | 0.543 |
| Greptile/base + Terra/high + Terra/xhigh | 150.7 | 198.5 | 2.836 | 1.137 |  |  | 0.327 |
| Greptile/base + 2× Terra/xhigh | 151.2 | 213.0 | 3.043 | 1.186 | yes | yes | 1.0 |
| Gemini/high | 199.5 | 73.0 | 1.043 | None |  |  | 0.0 |
| Gemini/high + Greptile/base | 199.5 | 149.0 | 2.129 | None |  |  | 0.0 |
| Gemini/high + Sol/high | 199.5 | 116.0 | 1.657 | None |  |  | 0.0 |
| Gemini/high + Sol/medium | 199.5 | 95.5 | 1.364 | None |  |  | 0.0 |
| Gemini/high + Terra/xhigh | 199.5 | 176.0 | 2.514 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/high | 199.5 | 164.0 | 2.343 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/medium | 199.5 | 155.5 | 2.221 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Terra/xhigh | 199.5 | 210.0 | 3.0 | None |  |  | 0.203 |
| Gemini/high + 2× Sol/high | 199.5 | 130.33 | 1.862 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Sol/medium | 199.5 | 121.0 | 1.729 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Terra/xhigh | 199.5 | 187.0 | 2.671 | None |  |  | 0.0 |
| Gemini/high + 2× Sol/medium | 199.5 | 105.0 | 1.5 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Terra/xhigh | 199.5 | 180.5 | 2.579 | None |  |  | 0.0 |
| Gemini/high + 2× Terra/xhigh | 199.5 | 202.0 | 2.886 | None |  |  | 0.027 |
| Gemini/high + Greptile/base + Terra/high | 201.8 | 176.67 | 2.524 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Terra/high | 201.8 | 149.06 | 2.129 | None |  |  | 0.0 |
| Gemini/high + Terra/high | 201.9 | 127.0 | 1.814 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Terra/high | 201.9 | 134.96 | 1.928 | None |  |  | 0.0 |
| Gemini/high + Terra/high + Terra/xhigh | 201.9 | 185.38 | 2.648 | None |  |  | 0.0 |
| Gemini/high + 2× Terra/high | 204.1 | 142.0 | 2.029 | None |  |  | 0.0 |
| Sol/xhigh | 211.5 | 124.0 | 1.771 | 0.33 |  |  | 0.0 |
| Sol/high + Sol/medium + Sol/xhigh | 211.5 | 133.92 | 1.913 | 0.597 |  |  | 0.0 |
| Sol/high + Sol/xhigh | 211.6 | 131.67 | 1.881 | 0.497 |  |  | 0.0 |
| 2× Sol/high + Sol/xhigh | 211.6 | 137.33 | 1.962 | 0.665 |  |  | 0.0 |
| Sol/medium + Sol/xhigh + Terra/xhigh | 211.6 | 191.62 | 2.737 | 0.523 |  |  | 0.0 |
| Sol/high + Sol/xhigh + Terra/xhigh | 211.7 | 192.75 | 2.754 | 0.59 |  |  | 0.0 |
| 2× Sol/medium + Sol/xhigh | 211.7 | 130.5 | 1.864 | 0.529 |  |  | 0.0 |
| Sol/xhigh + Terra/xhigh | 211.8 | 190.0 | 2.714 | 0.423 |  |  | 0.0 |
| Sol/xhigh + 2× Terra/xhigh | 211.8 | 211.5 | 3.021 | 0.516 |  | yes | 0.31 |
| Sol/medium + Sol/xhigh | 211.9 | 127.5 | 1.821 | 0.43 |  |  | 0.0 |
| Sol/xhigh + Terra/high | 213.2 | 159.0 | 2.271 | 0.373 |  |  | 0.0 |
| Sol/medium + Sol/xhigh + Terra/high | 213.2 | 160.79 | 2.297 | 0.473 |  |  | 0.0 |
| Sol/high + Sol/xhigh + Terra/high | 213.3 | 163.06 | 2.329 | 0.541 |  |  | 0.0 |
| Sol/xhigh + Terra/high + Terra/xhigh | 213.4 | 198.96 | 2.842 | 0.466 |  | yes | 0.0 |
| Sol/xhigh + 2× Terra/high | 214.8 | 172.5 | 2.464 | 0.417 |  |  | 0.0 |
| Greptile/base + Sol/xhigh | 216.2 | 164.0 | 2.343 | 1.33 |  |  | 0.0 |
| Greptile/base + Sol/xhigh + Terra/xhigh | 216.2 | 216.25 | 3.089 | 1.423 | yes | yes | 0.55 |
| Greptile/base + Sol/high + Sol/xhigh | 216.3 | 169.0 | 2.414 | 1.497 |  |  | 0.0 |
| Greptile/base + Sol/medium + Sol/xhigh | 216.4 | 165.75 | 2.368 | 1.43 |  |  | 0.0 |
| Greptile/base + Sol/xhigh + Terra/high | 217.3 | 187.67 | 2.681 | 1.373 |  |  | 0.0 |
| 2× Sol/xhigh | 226.0 | 150.0 | 2.143 | 0.659 |  |  | 0.0 |
| Sol/high + 2× Sol/xhigh | 226.0 | 153.67 | 2.195 | 0.827 |  |  | 0.0 |
| Sol/medium + 2× Sol/xhigh | 226.0 | 151.0 | 2.157 | 0.759 |  |  | 0.0 |
| 2× Sol/xhigh + Terra/xhigh | 226.0 | 208.0 | 2.971 | 0.752 |  |  | 0.033 |
| Greptile/base + 2× Sol/xhigh | 227.1 | 182.0 | 2.6 | 1.659 |  |  | 0.0 |
| 2× Sol/xhigh + Terra/high | 227.8 | 180.67 | 2.581 | 0.703 |  |  | 0.0 |
| Gemini/high + Sol/xhigh | 238.0 | 150.0 | 2.143 | None |  |  | 0.0 |
| Gemini/high + Sol/medium + Sol/xhigh | 238.0 | 152.5 | 2.179 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Sol/xhigh | 238.1 | 184.0 | 2.629 | None |  |  | 0.0 |
| Gemini/high + Sol/high + Sol/xhigh | 238.1 | 154.67 | 2.21 | None |  |  | 0.0 |
| Gemini/high + Sol/xhigh + Terra/xhigh | 238.1 | 206.25 | 2.946 | None |  |  | 0.007 |
| Gemini/high + Sol/xhigh + Terra/high | 238.8 | 177.75 | 2.539 | None |  |  | 0.0 |
| Gemini/high + 2× Sol/xhigh | 245.9 | 170.0 | 2.429 | None |  |  | 0.0 |
| Greptile/plus | 265.9 | 164.0 | 2.343 | 3.0 |  |  | 0.0 |
| Greptile/base + Greptile/plus | 265.9 | 173.0 | 2.471 | 4.0 |  |  | 0.0 |
| Greptile/plus + Sol/high | 265.9 | 174.33 | 2.49 | 3.168 |  |  | 0.0 |
| Greptile/plus + Sol/medium | 265.9 | 166.5 | 2.379 | 3.1 |  |  | 0.0 |
| Greptile/plus + Terra/high | 265.9 | 184.5 | 2.636 | 3.044 |  |  | 0.0 |
| Greptile/plus + Terra/xhigh | 265.9 | 218.0 | 3.114 | 3.093 |  | yes | 0.0 |
| Greptile/base + Greptile/plus + Sol/high | 265.9 | 181.0 | 2.586 | 4.168 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Sol/medium | 265.9 | 174.5 | 2.493 | 4.1 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Terra/high | 265.9 | 191.5 | 2.736 | 4.044 |  |  | 0.0 |
| Greptile/base + Greptile/plus + Terra/xhigh | 265.9 | 224.5 | 3.207 | 4.093 |  |  | 0.0 |
| Greptile/plus + 2× Sol/high | 265.9 | 178.67 | 2.552 | 3.335 |  |  | 0.0 |
| Greptile/plus + Sol/high + Sol/medium | 265.9 | 175.17 | 2.502 | 3.267 |  |  | 0.0 |
| Greptile/plus + Sol/high + Terra/high | 265.9 | 192.33 | 2.748 | 3.211 |  |  | 0.0 |
| Greptile/plus + Sol/high + Terra/xhigh | 265.9 | 222.83 | 3.183 | 3.26 |  |  | 0.0 |
| Greptile/plus + 2× Sol/medium | 265.9 | 168.0 | 2.4 | 3.2 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Terra/high | 265.9 | 186.17 | 2.66 | 3.143 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Terra/xhigh | 265.9 | 219.5 | 3.136 | 3.193 |  |  | 0.0 |
| Greptile/plus + 2× Terra/high | 265.9 | 193.0 | 2.757 | 3.087 |  |  | 0.0 |
| Greptile/plus + Terra/high + Terra/xhigh | 265.9 | 223.0 | 3.186 | 3.137 |  | yes | 0.0 |
| Greptile/plus + 2× Terra/xhigh | 265.9 | 236.0 | 3.371 | 3.186 | yes | yes | 0.997 |
| Gemini/high + Greptile/plus | 274.9 | 183.0 | 2.614 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Greptile/plus | 274.9 | 191.0 | 2.729 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/high | 274.9 | 190.33 | 2.719 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/medium | 274.9 | 185.0 | 2.643 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Terra/xhigh | 274.9 | 229.0 | 3.271 | None |  |  | 0.057 |
| Gemini/high + Greptile/plus + Terra/high | 275.7 | 201.33 | 2.876 | None |  |  | 0.0 |
| Greptile/plus + Sol/xhigh | 286.4 | 192.0 | 2.743 | 3.33 |  |  | 0.0 |
| Greptile/plus + Sol/high + Sol/xhigh | 286.5 | 194.83 | 2.783 | 3.497 |  |  | 0.0 |
| Greptile/plus + Sol/medium + Sol/xhigh | 286.5 | 192.75 | 2.754 | 3.43 |  |  | 0.0 |
| Greptile/plus + Sol/xhigh + Terra/xhigh | 286.5 | 235.0 | 3.357 | 3.423 |  |  | 0.36 |
| Greptile/base + Greptile/plus + Sol/xhigh | 286.6 | 198.0 | 2.829 | 4.33 |  |  | 0.0 |
| Greptile/plus + Sol/xhigh + Terra/high | 288.4 | 210.0 | 3.0 | 3.373 |  |  | 0.0 |
| Gemini/high + Greptile/plus + Sol/xhigh | 288.7 | 205.5 | 2.936 | None |  |  | 0.0 |
| Ember/xhigh | 292.4 | 29.0 | 0.414 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base | 292.4 | 122.0 | 1.743 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high | 292.4 | 90.33 | 1.29 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium | 292.4 | 61.0 | 0.871 | None |  |  | 0.0 |
| Ember/xhigh + Terra/xhigh | 292.4 | 158.5 | 2.264 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/high | 292.4 | 143.0 | 2.043 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/medium | 292.4 | 130.0 | 1.857 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Terra/xhigh | 292.4 | 193.5 | 2.764 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/high | 292.4 | 107.67 | 1.538 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Sol/medium | 292.4 | 95.33 | 1.362 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Terra/xhigh | 292.4 | 171.33 | 2.448 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/medium | 292.4 | 72.0 | 1.029 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Terra/xhigh | 292.4 | 163.5 | 2.336 | None |  |  | 0.0 |
| Ember/xhigh + 2× Terra/xhigh | 292.4 | 186.0 | 2.657 | None |  |  | 0.0 |
| Ember/xhigh + Terra/high | 296.5 | 96.17 | 1.374 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Terra/high | 296.5 | 153.17 | 2.188 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Terra/high | 296.5 | 106.46 | 1.521 | None |  |  | 0.0 |
| Ember/xhigh + Terra/high + Terra/xhigh | 296.5 | 168.54 | 2.408 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Terra/high | 296.6 | 127.28 | 1.818 | None |  |  | 0.0 |
| Greptile/plus + 2× Sol/xhigh | 297.4 | 206.0 | 2.943 | 3.659 |  |  | 0.0 |
| Ember/xhigh + 2× Terra/high | 300.7 | 114.83 | 1.64 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high | 304.5 | 84.0 | 1.2 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Greptile/base | 304.5 | 152.0 | 2.171 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/high | 304.5 | 123.0 | 1.757 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/medium | 304.5 | 102.5 | 1.464 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Terra/xhigh | 304.5 | 181.0 | 2.586 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Terra/high | 305.7 | 132.67 | 1.895 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Sol/xhigh | 319.5 | 153.0 | 2.186 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh + Terra/xhigh | 319.5 | 193.5 | 2.764 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh | 319.6 | 131.0 | 1.871 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Sol/xhigh | 319.6 | 167.5 | 2.393 | None |  |  | 0.0 |
| Ember/xhigh + Sol/high + Sol/xhigh | 319.7 | 137.33 | 1.962 | None |  |  | 0.0 |
| Ember/xhigh + Sol/medium + Sol/xhigh | 319.7 | 133.75 | 1.911 | None |  |  | 0.0 |
| Ember/xhigh + Sol/xhigh + Terra/high | 321.9 | 163.42 | 2.335 | None |  |  | 0.0 |
| Ember/xhigh + 2× Sol/xhigh | 333.3 | 154.0 | 2.2 | None |  |  | 0.0 |
| Kimi/high | 340.2 | 65.0 | 0.929 | None |  |  | 0.0 |
| Gemini/high + Kimi/high | 340.2 | 106.0 | 1.514 | None |  |  | 0.0 |
| Greptile/base + Kimi/high | 340.2 | 142.0 | 2.029 | None |  |  | 0.0 |
| Kimi/high + Sol/high | 340.2 | 105.33 | 1.505 | None |  |  | 0.0 |
| Kimi/high + Sol/medium | 340.2 | 87.0 | 1.243 | None |  |  | 0.0 |
| Kimi/high + Terra/xhigh | 340.2 | 169.0 | 2.414 | None |  |  | 0.0 |
| Gemini/high + Greptile/base + Kimi/high | 340.2 | 166.0 | 2.371 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/high | 340.2 | 132.0 | 1.886 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/medium | 340.2 | 119.0 | 1.7 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Terra/xhigh | 340.2 | 189.0 | 2.7 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/high | 340.2 | 156.0 | 2.229 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/medium | 340.2 | 147.5 | 2.107 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Terra/xhigh | 340.2 | 205.5 | 2.936 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/high | 340.2 | 120.0 | 1.714 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Sol/medium | 340.2 | 110.67 | 1.581 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Terra/xhigh | 340.2 | 179.67 | 2.567 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/medium | 340.2 | 96.0 | 1.371 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Terra/xhigh | 340.2 | 174.25 | 2.489 | None |  |  | 0.0 |
| Kimi/high + 2× Terra/xhigh | 340.2 | 196.0 | 2.8 | None |  |  | 0.0 |
| Kimi/high + Terra/high | 341.6 | 117.17 | 1.674 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Terra/high | 341.6 | 144.33 | 2.062 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Terra/high | 341.6 | 138.5 | 1.979 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Terra/high | 341.6 | 124.96 | 1.785 | None |  |  | 0.0 |
| Kimi/high + Terra/high + Terra/xhigh | 341.6 | 178.71 | 2.553 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Terra/high | 341.7 | 168.17 | 2.402 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus | 341.8 | 166.0 | 2.371 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Greptile/plus | 341.8 | 185.0 | 2.643 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Greptile/plus | 341.8 | 175.0 | 2.5 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/high | 341.8 | 176.33 | 2.519 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/medium | 341.8 | 168.5 | 2.407 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Terra/xhigh | 341.8 | 219.5 | 3.136 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh | 342.2 | 140.5 | 2.007 | None |  |  | 0.0 |
| Gemini/high + Kimi/high + Sol/xhigh | 342.3 | 160.0 | 2.286 | None |  |  | 0.0 |
| Greptile/base + Kimi/high + Sol/xhigh | 342.3 | 177.5 | 2.536 | None |  |  | 0.0 |
| Kimi/high + Sol/high + Sol/xhigh | 342.3 | 147.0 | 2.1 | None |  |  | 0.0 |
| Kimi/high + Sol/medium + Sol/xhigh | 342.3 | 143.5 | 2.05 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh + Terra/xhigh | 342.3 | 200.25 | 2.861 | None |  |  | 0.0 |
| Kimi/high + 2× Terra/high | 343.0 | 132.5 | 1.893 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Terra/high | 343.1 | 186.5 | 2.664 | None |  |  | 0.0 |
| Kimi/high + Sol/xhigh + Terra/high | 343.8 | 171.08 | 2.444 | None |  |  | 0.0 |
| Kimi/high + 2× Sol/xhigh | 344.4 | 164.0 | 2.343 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Sol/xhigh | 346.2 | 193.5 | 2.764 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high | 352.1 | 180.0 | 2.571 | None |  |  | 0.0 |
| Gemini/high + Greptile/plus + Kimi/high | 352.1 | 196.0 | 2.8 | None |  |  | 0.0 |
| Greptile/base + Greptile/plus + Kimi/high | 352.1 | 188.0 | 2.686 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/high | 352.1 | 186.33 | 2.662 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/medium | 352.1 | 182.0 | 2.6 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Terra/xhigh | 352.1 | 228.0 | 3.257 | None |  |  | 0.007 |
| Greptile/plus + Kimi/high + Terra/high | 354.3 | 197.5 | 2.821 | None |  |  | 0.0 |
| Greptile/plus + Kimi/high + Sol/xhigh | 355.4 | 202.0 | 2.886 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high | 386.7 | 74.0 | 1.057 | None |  |  | 0.0 |
| Ember/xhigh + Gemini/high + Kimi/high | 386.7 | 112.0 | 1.6 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/base + Kimi/high | 386.7 | 145.0 | 2.071 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/high | 386.7 | 110.33 | 1.576 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/medium | 386.7 | 92.0 | 1.314 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Terra/xhigh | 386.7 | 173.5 | 2.479 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Terra/high | 387.0 | 121.5 | 1.736 | None |  |  | 0.0 |
| Ember/xhigh + Kimi/high + Sol/xhigh | 390.2 | 143.5 | 2.05 | None |  |  | 0.0 |
| Ember/xhigh + Greptile/plus + Kimi/high | 393.4 | 182.0 | 2.6 | None |  |  | 0.0 |
