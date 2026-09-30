| scenario | delegated | expected child: model, status | reported | verify on branch | landed | wall | tokens root / children | every child (harness@depth: exit) | steer |
|---|---|---|---|---|---|---|---|---|---|
| s3 | y | opencode: google/gemini-3.1-flash-lite, Ok | y | pass | y | 99s | 35911 / 301566 | opencode@1: Ok | - |
| s5 | y | pi: gemini-3.5-flash, Failed | n | none | n | 66s | 36179 / 0 | pi@1: Failed | - |
| s5 | y | pi (never ran): default model, None | n | none | n | 420s (timed out) | 16395 / 0 (+1 unrecorded) | acp@1: None | - |
| s5 | y | pi: gemini-flash-latest, Failed | n | none | n | 295s | 59879 / 409794 | pi@1: Failed, claude-code@1: Ok | - |
| s5 | y | pi: gemini-3.1-flash-lite, Failed | n | none | n | 72s | 35957 / 67171 | pi@1: Failed | - |
| s5 | y | pi: deepseek-v4-flash, Failed | n | none | n | 33s | 35425 / 0 | pi@1: Failed | - |
| s5 | y | pi: minimax-m3, Failed | n | none | n | 67s | 35915 / 0 | pi@1: Failed | - |
| s5 | y | pi: gpt-5-mini, Failed | n | none | n | 83s | 56184 / 71678 | pi@1: Failed, claude-code@1: Ok | - |
| s5 | y | pi (never ran): claude-opus-5-5[1m], Ok | y | pass | y | 87s | 54402 / 68268 (+1 unrecorded) | pi@1: None, claude-code@1: Ok | - |
| s5 | y | pi: gemini-3.5-flash, Failed | n | none | n | 90s | 57476 / 53333 | pi@1: Failed, claude-code@1: Ok | - |
| s5 | y | pi: gemini-2.5-flash, Failed | n | pass | y | 99s | 190324 / 338828 (+1 unrecorded) | pi@1: Failed, pi@2: Failed, gemini@2: Unreported | - |
| s5 | y | pi: gemini-3.1-flash-lite, Failed | n | none | n | 115s | 100376 / 137394 | pi@1: Failed, claude-code@1: Ok | - |
| s5 | y | pi: gemini-3.1-flash-lite, Failed | n | none | n | 76s | 36688 / 0 | pi@1: Failed | - |
| s6 | y | codex: default model, Ok | y | pass | y | 136s | 167395 / 267207 | codex@1: Ok | - |
| s7 | y | codex: default model, Ok | y | pass | y | 137s | 30408 / 354623 | codex@1: Ok | - |
| s7 | y | codex (never ran): default model, Ok | y | pass | y | 126s | 27149 / 31226 | acp@1: Ok | - |
