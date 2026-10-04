# D-02: cold start, before and after

Two pairs of runs on the same machine (4-CPU Linux container, release builds, fake provider, 30 measured requests per application after 3 warm-up). Before is `d92975a` (the A-04 client: every client that finds no broker starts the companion, looking for it after 10, 30, 70 ms); after is `a8dd5af` (one start claim, looking every millisecond at first). `target/release/seatline-bench compare` output, unedited.

## Pair 1

before: before D-02 (A-04 client) (`d92975a7ae42`, harness release, companion release)  
after: after D-02 (start claim, 1 ms polling), run 1 (`a8dd5af86c4a`, harness release, companion release)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 12.0 → 3.1 | -8.9 | 12.4 → 3.3 | -9.1 |
| cold-broker | bench-a | submit→text | 6.1 → 6.1 | +0.0 | 6.4 → 6.5 | +0.1 |
| cold-broker | bench-a | start→text | 18.1 → 9.1 | -9.0 | 18.8 → 9.7 | -9.1 |
| cold-broker | bench-a | submit→done | 11.3 → 11.3 | -0.0 | 11.5 → 11.7 | +0.1 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| cold-broker | bench-a | init | 5.7 → 5.7 | +0.0 | 5.9 → 5.9 | -0.0 |
| cold-broker | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| cold-broker | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | +0.0 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| cold-three-app | bench-a | prepare | 11.9 → 5.9 | -6.0 | 13.4 → 7.0 | -6.4 |
| cold-three-app | bench-a | submit→text | 6.7 → 7.0 | +0.3 | 17.6 → 18.5 | +0.9 |
| cold-three-app | bench-a | start→text | 18.7 → 13.2 | -5.5 | 29.7 → 24.6 | -5.1 |
| cold-three-app | bench-a | submit→done | 11.9 → 12.4 | +0.5 | 22.8 → 23.6 | +0.8 |
| cold-three-app | bench-a | queue wait | 0.1 → 0.6 | +0.6 | 11.3 → 11.4 | +0.0 |
| cold-three-app | bench-a | init | 5.7 → 5.7 | -0.0 | 11.5 → 6.7 | -4.7 |
| cold-three-app | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| cold-three-app | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | +0.0 |
| cold-three-app | bench-a | cleanup | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | 0.0 |
| cold-three-app | bench-b | prepare | 11.9 → 5.8 | -6.0 | 13.4 → 6.1 | -7.3 |
| cold-three-app | bench-b | submit→text | 6.7 → 6.6 | -0.1 | 17.5 → 17.8 | +0.3 |
| cold-three-app | bench-b | start→text | 18.6 → 12.4 | -6.2 | 30.7 → 23.9 | -6.8 |
| cold-three-app | bench-b | submit→done | 11.9 → 11.8 | -0.1 | 22.6 → 23.0 | +0.4 |
| cold-three-app | bench-b | queue wait | 0.5 → 0.0 | -0.4 | 11.4 → 10.5 | -1.0 |
| cold-three-app | bench-b | init | 6.0 → 6.0 | -0.0 | 10.9 → 6.4 | -4.5 |
| cold-three-app | bench-b | text wait | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | +0.0 |
| cold-three-app | bench-b | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | -0.0 |
| cold-three-app | bench-b | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| cold-three-app | bench-c | prepare | 11.8 → 5.7 | -6.1 | 13.8 → 6.1 | -7.7 |
| cold-three-app | bench-c | submit→text | 6.6 → 6.7 | +0.0 | 17.6 → 17.8 | +0.3 |
| cold-three-app | bench-c | start→text | 18.4 → 12.4 | -6.0 | 29.9 → 23.6 | -6.3 |
| cold-three-app | bench-c | submit→done | 11.7 → 11.7 | -0.0 | 22.7 → 23.0 | +0.2 |
| cold-three-app | bench-c | queue wait | 0.5 → 0.5 | -0.0 | 11.1 → 10.9 | -0.1 |
| cold-three-app | bench-c | init | 5.7 → 5.7 | +0.0 | 6.2 → 10.9 | +4.8 |
| cold-three-app | bench-c | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| cold-three-app | bench-c | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | -0.0 |
| cold-three-app | bench-c | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |

## Pair 2

before: before D-02 (A-04 client), repeat (`d92975a7ae42`, harness release, companion release)  
after: after D-02 (start claim, 1 ms polling), run 2 (`a8dd5af86c4a`, harness release, companion release)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 12.0 → 3.1 | -8.9 | 12.2 → 3.3 | -8.9 |
| cold-broker | bench-a | submit→text | 6.1 → 6.1 | -0.1 | 6.4 → 6.7 | +0.2 |
| cold-broker | bench-a | start→text | 18.2 → 9.2 | -9.0 | 18.5 → 10.4 | -8.1 |
| cold-broker | bench-a | submit→done | 11.3 → 11.2 | -0.0 | 11.7 → 11.4 | -0.3 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | 0.0 |
| cold-broker | bench-a | init | 5.7 → 5.7 | -0.0 | 6.0 → 6.1 | +0.1 |
| cold-broker | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| cold-broker | bench-a | completion | 5.2 → 5.2 | -0.0 | 5.3 → 5.2 | -0.1 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| cold-three-app | bench-a | prepare | 11.9 → 6.0 | -5.9 | 14.0 → 6.6 | -7.4 |
| cold-three-app | bench-a | submit→text | 6.6 → 6.8 | +0.2 | 17.9 → 18.1 | +0.2 |
| cold-three-app | bench-a | start→text | 18.5 → 13.0 | -5.5 | 30.0 → 24.1 | -5.9 |
| cold-three-app | bench-a | submit→done | 11.8 → 11.9 | +0.1 | 23.1 → 23.2 | +0.1 |
| cold-three-app | bench-a | queue wait | 0.0 → 0.6 | +0.5 | 11.9 → 11.5 | -0.3 |
| cold-three-app | bench-a | init | 5.7 → 5.8 | +0.1 | 11.2 → 10.5 | -0.7 |
| cold-three-app | bench-a | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| cold-three-app | bench-a | completion | 5.2 → 5.2 | +0.0 | 5.9 → 5.3 | -0.7 |
| cold-three-app | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| cold-three-app | bench-b | prepare | 11.9 → 5.9 | -6.0 | 14.1 → 6.1 | -8.0 |
| cold-three-app | bench-b | submit→text | 7.0 → 6.7 | -0.2 | 17.3 → 17.6 | +0.3 |
| cold-three-app | bench-b | start→text | 19.4 → 12.7 | -6.7 | 29.7 → 23.6 | -6.1 |
| cold-three-app | bench-b | submit→done | 12.0 → 11.8 | -0.2 | 22.5 → 22.7 | +0.2 |
| cold-three-app | bench-b | queue wait | 0.4 → 0.1 | -0.4 | 11.1 → 11.4 | +0.3 |
| cold-three-app | bench-b | init | 5.8 → 5.9 | +0.1 | 8.2 → 10.7 | +2.6 |
| cold-three-app | bench-b | text wait | 0.0 → 0.0 | +0.0 | 0.1 → 0.0 | -0.0 |
| cold-three-app | bench-b | completion | 5.2 → 5.2 | -0.0 | 5.3 → 5.2 | -0.0 |
| cold-three-app | bench-b | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| cold-three-app | bench-c | prepare | 11.8 → 5.8 | -6.1 | 14.6 → 7.0 | -7.6 |
| cold-three-app | bench-c | submit→text | 7.0 → 6.8 | -0.2 | 17.5 → 18.3 | +0.8 |
| cold-three-app | bench-c | start→text | 18.8 → 12.6 | -6.2 | 30.1 → 24.7 | -5.4 |
| cold-three-app | bench-c | submit→done | 11.9 → 11.8 | -0.1 | 22.8 → 23.5 | +0.8 |
| cold-three-app | bench-c | queue wait | 0.4 → 0.5 | +0.0 | 10.9 → 11.7 | +0.8 |
| cold-three-app | bench-c | init | 5.8 → 5.7 | -0.1 | 10.7 → 10.7 | +0.0 |
| cold-three-app | bench-c | text wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| cold-three-app | bench-c | completion | 5.2 → 5.2 | +0.0 | 5.3 → 5.3 | -0.0 |
| cold-three-app | bench-c | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
