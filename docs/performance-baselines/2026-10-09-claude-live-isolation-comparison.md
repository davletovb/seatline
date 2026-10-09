before: claude live, launch isolation off (run 1) (`ad026ce5dc42`, harness release, companion release)  
after: claude live, launch isolation on (`ad026ce5dc42`, harness release, companion release, policy `{"claude_isolation":true}`)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 3.4 → 3.4 | -0.0 | 6.1 → 4.5 | -1.6 |
| cold-broker | bench-a | submit→text | 1912.2 → 1886.8 | -25.3 | 3211.9 → 2108.7 | -1103.2 |
| cold-broker | bench-a | start→text | 1915.4 → 1889.7 | -25.7 | 3215.9 → 2112.0 | -1103.9 |
| cold-broker | bench-a | submit→done | 2386.5 → 2392.4 | +5.9 | 3713.8 → 2687.1 | -1026.8 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| cold-broker | bench-a | init | 614.8 → 578.6 | -36.2 | 657.4 → 655.2 | -2.2 |
| cold-broker | bench-a | text wait | 1296.3 → 1266.6 | -29.7 | 2577.8 → 1482.9 | -1094.9 |
| cold-broker | bench-a | completion | 501.9 → 537.7 | +35.8 | 620.7 → 677.6 | +56.8 |
| cold-broker | bench-a | tail | 475.8 → 506.3 | +30.5 | 570.8 → 652.0 | +81.2 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
| warm-send | bench-a | prepare | 0.4 → 0.4 | +0.1 | 0.6 → 0.5 | -0.0 |
| warm-send | bench-a | submit→text | 1854.6 → 1882.2 | +27.6 | 2553.8 → 2241.0 | -312.8 |
| warm-send | bench-a | start→text | 1855.0 → 1882.6 | +27.6 | 2554.4 → 2241.3 | -313.1 |
| warm-send | bench-a | submit→done | 2382.5 → 2349.9 | -32.6 | 3001.9 → 2693.0 | -308.9 |
| warm-send | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| warm-send | bench-a | init | 574.1 → 584.2 | +10.1 | 664.4 → 730.7 | +66.3 |
| warm-send | bench-a | text wait | 1284.2 → 1274.9 | -9.4 | 1956.2 → 1546.1 | -410.2 |
| warm-send | bench-a | completion | 475.6 → 452.0 | -23.5 | 689.9 → 690.7 | +0.8 |
| warm-send | bench-a | tail | 449.2 → 425.8 | -23.4 | 667.7 → 668.5 | +0.8 |
| warm-send | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-probe | bench-a | prepare | 0.4 → 0.4 | -0.0 | 0.5 → 1.3 | +0.8 |
| warm-send-probe | bench-a | submit→text | 2110.3 → 2159.7 | +49.4 | 3111.2 → 2391.3 | -719.9 |
| warm-send-probe | bench-a | start→text | 2110.9 → 2160.0 | +49.1 | 3111.8 → 2392.7 | -719.1 |
| warm-send-probe | bench-a | submit→done | 2644.2 → 2656.4 | +12.2 | 3640.7 → 2921.2 | -719.5 |
| warm-send-probe | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-probe | bench-a | probe | 210.5 → 216.3 | +5.8 | 246.0 → 287.7 | +41.6 |
| warm-send-probe | bench-a | init | 568.5 → 585.3 | +16.9 | 669.4 → 805.9 | +136.5 |
| warm-send-probe | bench-a | text wait | 1285.6 → 1238.6 | -47.0 | 2366.1 → 1520.5 | -845.6 |
| warm-send-probe | bench-a | completion | 545.1 → 516.2 | -28.9 | 670.3 → 572.1 | -98.2 |
| warm-send-probe | bench-a | tail | 523.2 → 467.0 | -56.2 | 628.4 → 524.0 | -104.4 |
| warm-send-probe | bench-a | cleanup | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| prepared-send | bench-a | prepare | 0.3 → 0.3 | -0.0 | 1.7 → 0.4 | -1.3 |
| prepared-send | bench-a | submit→text | 1908.6 → 1856.6 | -52.0 | 2098.1 → 2023.8 | -74.3 |
| prepared-send | bench-a | start→text | 1909.0 → 1857.0 | -52.0 | 2098.3 → 2024.0 | -74.3 |
| prepared-send | bench-a | submit→done | 2501.2 → 2402.2 | -99.0 | 2703.2 → 2718.1 | +14.9 |
| prepared-send | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| prepared-send | bench-a | init | 630.5 → 593.0 | -37.5 | 729.3 → 653.9 | -75.4 |
| prepared-send | bench-a | text wait | 1298.8 → 1236.5 | -62.3 | 1440.9 → 1415.9 | -25.0 |
| prepared-send | bench-a | completion | 530.2 → 524.7 | -5.5 | 715.9 → 823.8 | +107.9 |
| prepared-send | bench-a | tail | 498.8 → 500.6 | +1.8 | 689.4 → 801.4 | +112.1 |
| prepared-send | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-status | bench-a | prepare | 0.3 → 0.3 | -0.0 | 0.8 → 0.6 | -0.2 |
| warm-status | bench-a | submit→done | 239.9 → 224.2 | -15.7 | 274.5 → 269.7 | -4.8 |
| warm-status | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| warm-status | bench-a | probe | 239.3 → 223.7 | -15.6 | 274.1 → 269.2 | -4.8 |
| warm-status | bench-a | completion | 0.1 → 0.1 | +0.0 | 0.2 → 0.2 | -0.0 |
| warm-status | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| resumed-context | bench-a | prepare | 0.4 → 0.4 | +0.0 | 0.6 → 0.8 | +0.2 |
| resumed-context | bench-a | submit→text | 1942.1 → 1887.3 | -54.8 | 2014.4 → 2370.9 | +356.5 |
| resumed-context | bench-a | start→text | 1942.6 → 1887.8 | -54.8 | 2015.0 → 2371.2 | +356.2 |
| resumed-context | bench-a | submit→done | 2459.2 → 2430.7 | -28.6 | 2542.5 → 2861.6 | +319.1 |
| resumed-context | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| resumed-context | bench-a | init | 587.1 → 558.5 | -28.6 | 649.2 → 701.9 | +52.7 |
| resumed-context | bench-a | text wait | 1359.9 → 1333.8 | -26.2 | 1404.0 → 1882.2 | +478.2 |
| resumed-context | bench-a | completion | 501.2 → 518.1 | +16.9 | 638.7 → 686.2 | +47.5 |
| resumed-context | bench-a | tail | 469.4 → 495.5 | +26.1 | 601.6 → 644.1 | +42.5 |
| resumed-context | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
