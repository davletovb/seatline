before: claude live, launch isolation off (run 1) (`ad026ce5dc42`, harness release, companion release)  
after: claude live, launch isolation off (run 2) (`ad026ce5dc42`, harness release, companion release)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 3.4 → 3.3 | -0.1 | 6.1 → 4.3 | -1.8 |
| cold-broker | bench-a | submit→text | 1912.2 → 1995.9 | +83.7 | 3211.9 → 4654.4 | +1442.5 |
| cold-broker | bench-a | start→text | 1915.4 → 1998.9 | +83.5 | 3215.9 → 4658.2 | +1442.3 |
| cold-broker | bench-a | submit→done | 2386.5 → 2537.9 | +151.4 | 3713.8 → 5105.4 | +1391.6 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | 0.0 |
| cold-broker | bench-a | init | 614.8 → 607.1 | -7.7 | 657.4 → 710.5 | +53.1 |
| cold-broker | bench-a | text wait | 1296.3 → 1323.8 | +27.5 | 2577.8 → 3943.5 | +1365.7 |
| cold-broker | bench-a | completion | 501.9 → 516.6 | +14.7 | 620.7 → 655.8 | +35.1 |
| cold-broker | bench-a | tail | 475.8 → 485.2 | +9.3 | 570.8 → 624.3 | +53.5 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| warm-send | bench-a | prepare | 0.4 → 0.4 | +0.0 | 0.6 → 0.7 | +0.1 |
| warm-send | bench-a | submit→text | 1854.6 → 1867.1 | +12.5 | 2553.8 → 3291.2 | +737.4 |
| warm-send | bench-a | start→text | 1855.0 → 1867.5 | +12.5 | 2554.4 → 3291.6 | +737.2 |
| warm-send | bench-a | submit→done | 2382.5 → 2402.1 | +19.6 | 3001.9 → 3807.5 | +805.6 |
| warm-send | bench-a | queue wait | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-send | bench-a | init | 574.1 → 617.5 | +43.4 | 664.4 → 717.9 | +53.5 |
| warm-send | bench-a | text wait | 1284.2 → 1238.5 | -45.7 | 1956.2 → 2572.9 | +616.7 |
| warm-send | bench-a | completion | 475.6 → 516.3 | +40.7 | 689.9 → 602.1 | -87.8 |
| warm-send | bench-a | tail | 449.2 → 494.9 | +45.7 | 667.7 → 579.4 | -88.3 |
| warm-send | bench-a | cleanup | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-probe | bench-a | prepare | 0.4 → 0.4 | -0.0 | 0.5 → 2.3 | +1.8 |
| warm-send-probe | bench-a | submit→text | 2110.3 → 2208.4 | +98.1 | 3111.2 → 5816.0 | +2704.7 |
| warm-send-probe | bench-a | start→text | 2110.9 → 2208.9 | +98.0 | 3111.8 → 5816.4 | +2704.7 |
| warm-send-probe | bench-a | submit→done | 2644.2 → 2756.7 | +112.6 | 3640.7 → 6300.4 | +2659.8 |
| warm-send-probe | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| warm-send-probe | bench-a | probe | 210.5 → 227.5 | +17.0 | 246.0 → 252.9 | +6.9 |
| warm-send-probe | bench-a | init | 568.5 → 620.9 | +52.5 | 669.4 → 718.1 | +48.7 |
| warm-send-probe | bench-a | text wait | 1285.6 → 1310.8 | +25.2 | 2366.1 → 4964.6 | +2598.5 |
| warm-send-probe | bench-a | completion | 545.1 → 534.0 | -11.1 | 670.3 → 853.7 | +183.5 |
| warm-send-probe | bench-a | tail | 523.2 → 510.0 | -13.2 | 628.4 → 789.0 | +160.6 |
| warm-send-probe | bench-a | cleanup | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | 0.0 |
| prepared-send | bench-a | prepare | 0.3 → 0.3 | 0.0 | 1.7 → 0.7 | -1.0 |
| prepared-send | bench-a | submit→text | 1908.6 → 1915.4 | +6.8 | 2098.1 → 8914.1 | +6816.0 |
| prepared-send | bench-a | start→text | 1909.0 → 1916.1 | +7.1 | 2098.3 → 8914.3 | +6816.0 |
| prepared-send | bench-a | submit→done | 2501.2 → 2457.7 | -43.4 | 2703.2 → 9399.6 | +6696.4 |
| prepared-send | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| prepared-send | bench-a | init | 630.5 → 661.0 | +30.5 | 729.3 → 703.3 | -26.0 |
| prepared-send | bench-a | text wait | 1298.8 → 1259.9 | -38.9 | 1440.9 → 8263.7 | +6822.8 |
| prepared-send | bench-a | completion | 530.2 → 531.4 | +1.2 | 715.9 → 635.6 | -80.3 |
| prepared-send | bench-a | tail | 498.8 → 495.1 | -3.7 | 689.4 → 599.0 | -90.4 |
| prepared-send | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | -0.0 |
| warm-status | bench-a | prepare | 0.3 → 0.4 | +0.0 | 0.8 → 0.5 | -0.3 |
| warm-status | bench-a | submit→done | 239.9 → 245.0 | +5.1 | 274.5 → 286.4 | +11.8 |
| warm-status | bench-a | queue wait | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | -0.0 |
| warm-status | bench-a | probe | 239.3 → 244.5 | +5.2 | 274.1 → 285.8 | +11.7 |
| warm-status | bench-a | completion | 0.1 → 0.1 | +0.0 | 0.2 → 2.4 | +2.1 |
| warm-status | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | 0.0 |
| resumed-context | bench-a | prepare | 0.4 → 0.4 | -0.0 | 0.6 → 4.4 | +3.8 |
| resumed-context | bench-a | submit→text | 1942.1 → 1913.2 | -28.9 | 2014.4 → 3371.6 | +1357.2 |
| resumed-context | bench-a | start→text | 1942.6 → 1913.6 | -29.1 | 2015.0 → 3371.9 | +1356.9 |
| resumed-context | bench-a | submit→done | 2459.2 → 2510.5 | +51.3 | 2542.5 → 3954.1 | +1411.6 |
| resumed-context | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| resumed-context | bench-a | init | 587.1 → 635.3 | +48.1 | 649.2 → 2110.8 | +1461.6 |
| resumed-context | bench-a | text wait | 1359.9 → 1284.1 | -75.8 | 1404.0 → 1410.4 | +6.4 |
| resumed-context | bench-a | completion | 501.2 → 550.1 | +48.9 | 638.7 → 2041.0 | +1402.3 |
| resumed-context | bench-a | tail | 469.4 → 522.8 | +53.4 | 601.6 → 2015.9 | +1414.3 |
| resumed-context | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
