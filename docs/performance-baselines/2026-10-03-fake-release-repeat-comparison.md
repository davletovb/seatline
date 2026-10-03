before: fake provider, release build, run 1 (`071ab6d662ca`, release build)  
after: fake provider, release build, run 2 (`071ab6d662ca`, release build)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 102.2 → 102.3 | +0.1 | 102.4 → 102.6 | +0.2 |
| cold-broker | bench-a | submit→text | 6.1 → 6.2 | +0.1 | 6.3 → 6.6 | +0.2 |
| cold-broker | bench-a | start→text | 108.4 → 108.6 | +0.2 | 108.7 → 109.0 | +0.3 |
| cold-broker | bench-a | submit→done | 11.3 → 11.4 | +0.1 | 11.5 → 11.7 | +0.2 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| cold-broker | bench-a | init | 5.7 → 5.8 | +0.1 | 5.9 → 6.1 | +0.2 |
| cold-broker | bench-a | text wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| cold-broker | bench-a | completion | 5.2 → 5.2 | -0.0 | 5.2 → 5.3 | +0.1 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-send | bench-a | prepare | 0.3 → 0.3 | +0.0 | 0.4 → 0.4 | -0.0 |
| warm-send | bench-a | submit→text | 5.9 → 5.9 | +0.0 | 6.1 → 6.1 | -0.0 |
| warm-send | bench-a | start→text | 6.2 → 6.3 | +0.1 | 6.5 → 6.5 | -0.0 |
| warm-send | bench-a | submit→done | 11.0 → 11.1 | +0.1 | 11.2 → 11.3 | +0.1 |
| warm-send | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| warm-send | bench-a | init | 5.5 → 5.5 | +0.0 | 5.6 → 5.6 | -0.0 |
| warm-send | bench-a | text wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| warm-send | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.2 → 5.2 | +0.0 |
| warm-send | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-send-adapter | bench-a | submit→text | 6.3 → 6.4 | +0.1 | 6.6 → 6.8 | +0.2 |
| warm-send-adapter | bench-a | start→text | 6.3 → 6.4 | +0.1 | 6.6 → 6.8 | +0.2 |
| warm-send-adapter | bench-a | submit→done | 11.5 → 11.6 | +0.1 | 11.7 → 11.9 | +0.2 |
| warm-send-adapter | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-adapter | bench-a | init | 5.5 → 5.5 | +0.0 | 5.6 → 5.7 | +0.1 |
| warm-send-adapter | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-adapter | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.2 → 5.2 | +0.0 |
| warm-send-adapter | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-send-probe | bench-a | prepare | 0.3 → 0.3 | -0.0 | 0.4 → 0.4 | +0.1 |
| warm-send-probe | bench-a | submit→text | 16.6 → 16.5 | -0.0 | 17.2 → 16.8 | -0.4 |
| warm-send-probe | bench-a | start→text | 16.9 → 16.8 | -0.1 | 17.7 → 17.1 | -0.6 |
| warm-send-probe | bench-a | submit→done | 21.7 → 21.7 | +0.0 | 22.2 → 21.9 | -0.3 |
| warm-send-probe | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-send-probe | bench-a | probe | 10.6 → 10.6 | -0.1 | 10.7 → 10.7 | -0.0 |
| warm-send-probe | bench-a | init | 5.6 → 5.6 | -0.0 | 5.7 → 5.7 | -0.1 |
| warm-send-probe | bench-a | text wait | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | 0.0 |
| warm-send-probe | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.2 → 5.2 | +0.0 |
| warm-send-probe | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-status | bench-a | prepare | 0.3 → 0.3 | +0.0 | 0.4 → 0.5 | +0.1 |
| warm-status | bench-a | submit→done | 10.9 → 11.0 | +0.1 | 11.2 → 11.4 | +0.1 |
| warm-status | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| warm-status | bench-a | probe | 10.6 → 10.7 | +0.1 | 10.8 → 10.9 | +0.1 |
| warm-status | bench-a | completion | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-status | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| resumed-context | bench-a | prepare | 0.3 → 0.4 | +0.1 | 0.4 → 0.5 | +0.1 |
| resumed-context | bench-a | submit→text | 5.9 → 5.9 | +0.0 | 6.0 → 6.1 | +0.0 |
| resumed-context | bench-a | start→text | 6.2 → 6.3 | +0.1 | 6.4 → 6.6 | +0.2 |
| resumed-context | bench-a | submit→done | 11.0 → 11.1 | +0.1 | 11.1 → 11.3 | +0.2 |
| resumed-context | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| resumed-context | bench-a | init | 5.5 → 5.5 | -0.0 | 5.6 → 5.6 | +0.0 |
| resumed-context | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| resumed-context | bench-a | completion | 5.2 → 5.2 | 0.0 | 5.2 → 5.2 | -0.0 |
| resumed-context | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | 0.0 |
| short-isolated-paced | bench-a | prepare | 0.5 → 0.5 | +0.0 | 0.6 → 0.7 | +0.0 |
| short-isolated-paced | bench-a | submit→text | 5.9 → 6.0 | +0.1 | 6.2 → 6.2 | +0.0 |
| short-isolated-paced | bench-a | start→text | 6.4 → 6.5 | +0.1 | 6.8 → 6.8 | +0.0 |
| short-isolated-paced | bench-a | submit→done | 11.1 → 11.1 | +0.1 | 11.5 → 11.4 | -0.1 |
| short-isolated-paced | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| short-isolated-paced | bench-a | init | 5.5 → 5.6 | +0.1 | 5.7 → 5.8 | +0.1 |
| short-isolated-paced | bench-a | text wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| short-isolated-paced | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | +0.0 |
| short-isolated-paced | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| three-app-short | bench-a | prepare | 0.4 → 0.5 | +0.2 | 1.7 → 1.6 | -0.1 |
| three-app-short | bench-a | submit→text | 7.4 → 6.4 | -1.1 | 23.2 → 17.0 | -6.2 |
| three-app-short | bench-a | start→text | 9.0 → 7.3 | -1.8 | 23.7 → 17.4 | -6.3 |
| three-app-short | bench-a | submit→done | 11.5 → 11.1 | -0.4 | 23.2 → 22.5 | -0.7 |
| three-app-short | bench-a | queue wait | 0.1 → 0.1 | -0.0 | 10.8 → 10.6 | -0.2 |
| three-app-short | bench-a | init | 5.6 → 5.5 | -0.0 | 12.0 → 6.7 | -5.3 |
| three-app-short | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| three-app-short | bench-a | completion | 5.2 → 5.2 | -0.0 | 5.6 → 5.5 | -0.0 |
| three-app-short | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| three-app-short | bench-b | prepare | 0.4 → 0.4 | +0.0 | 1.7 → 1.6 | -0.2 |
| three-app-short | bench-b | submit→text | 6.4 → 6.9 | +0.5 | 19.4 → 17.1 | -2.3 |
| three-app-short | bench-b | start→text | 7.6 → 7.9 | +0.3 | 19.6 → 17.6 | -1.9 |
| three-app-short | bench-b | submit→done | 11.3 → 11.3 | -0.0 | 22.7 → 22.2 | -0.5 |
| three-app-short | bench-b | queue wait | 0.0 → 0.3 | +0.3 | 10.8 → 10.7 | -0.1 |
| three-app-short | bench-b | init | 5.8 → 5.4 | -0.4 | 10.7 → 6.4 | -4.3 |
| three-app-short | bench-b | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| three-app-short | bench-b | completion | 5.2 → 5.2 | -0.0 | 5.3 → 5.5 | +0.2 |
| three-app-short | bench-b | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| three-app-short | bench-c | prepare | 0.4 → 0.4 | +0.1 | 1.7 → 1.7 | -0.1 |
| three-app-short | bench-c | submit→text | 11.4 → 6.0 | -5.4 | 18.0 → 17.3 | -0.8 |
| three-app-short | bench-c | start→text | 12.8 → 7.3 | -5.5 | 18.5 → 17.7 | -0.8 |
| three-app-short | bench-c | submit→done | 12.4 → 11.0 | -1.5 | 23.2 → 18.7 | -4.5 |
| three-app-short | bench-c | queue wait | 5.3 → 0.0 | -5.2 | 10.9 → 10.7 | -0.2 |
| three-app-short | bench-c | init | 5.6 → 5.4 | -0.2 | 6.4 → 6.5 | +0.1 |
| three-app-short | bench-c | text wait | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | +0.0 |
| three-app-short | bench-c | completion | 5.2 → 5.1 | -0.1 | 10.5 → 5.2 | -5.2 |
| three-app-short | bench-c | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| short-contended | bench-a | prepare | 0.5 → 0.5 | -0.0 | 0.6 → 0.7 | +0.0 |
| short-contended | bench-a | submit→text | 105.4 → 110.5 | +5.0 | 125.9 → 124.9 | -1.0 |
| short-contended | bench-a | start→text | 106.1 → 111.1 | +5.0 | 126.5 → 125.4 | -1.1 |
| short-contended | bench-a | submit→done | 110.6 → 115.8 | +5.2 | 131.1 → 130.1 | -1.0 |
| short-contended | bench-a | queue wait | 99.0 → 104.2 | +5.1 | 119.5 → 118.7 | -0.8 |
| short-contended | bench-a | init | 6.0 → 6.0 | +0.0 | 6.3 → 6.2 | -0.1 |
| short-contended | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| short-contended | bench-a | completion | 5.2 → 5.2 | -0.0 | 5.3 → 5.3 | +0.0 |
| short-contended | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| short-contended | bench-b | prepare | 0.4 → 0.4 | -0.0 | 0.5 → 1.4 | +0.8 |
| short-contended | bench-b | submit→text | 16.4 → 16.3 | -0.1 | 16.7 → 16.7 | +0.0 |
| short-contended | bench-b | start→text | 16.8 → 16.8 | -0.0 | 17.2 → 17.4 | +0.2 |
| short-contended | bench-b | submit→done | 320.1 → 320.4 | +0.3 | 322.8 → 321.5 | -1.3 |
| short-contended | bench-b | queue wait | 10.4 → 10.4 | -0.0 | 10.6 → 10.6 | -0.0 |
| short-contended | bench-b | init | 5.6 → 5.6 | 0.0 | 5.9 → 5.8 | -0.1 |
| short-contended | bench-b | text wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| short-contended | bench-b | completion | 303.8 → 304.3 | +0.5 | 306.2 → 305.0 | -1.2 |
| short-contended | bench-b | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| short-contended | bench-c | prepare | 0.4 → 0.4 | +0.0 | 0.7 → 0.6 | -0.1 |
| short-contended | bench-c | submit→text | 16.3 → 16.4 | +0.1 | 16.6 → 16.8 | +0.1 |
| short-contended | bench-c | start→text | 16.7 → 16.8 | +0.1 | 17.2 → 17.2 | +0.0 |
| short-contended | bench-c | submit→done | 320.2 → 320.5 | +0.4 | 322.9 → 322.2 | -0.7 |
| short-contended | bench-c | queue wait | 10.4 → 10.4 | +0.0 | 10.6 → 10.6 | +0.0 |
| short-contended | bench-c | init | 5.6 → 5.6 | +0.0 | 5.8 → 5.8 | +0.0 |
| short-contended | bench-c | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| short-contended | bench-c | completion | 303.8 → 304.0 | +0.2 | 306.6 → 305.8 | -0.7 |
| short-contended | bench-c | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
