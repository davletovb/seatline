before: claude live, launch isolation off (run 1) (`ad026ce5dc42`, harness release, companion release)  
after: claude live, after I-05 (process left to exit) (`7fbe37dd6583`, harness release, companion release)

| scenario | app | metric | p50 before → after (ms) | Δ p50 | p95 before → after (ms) | Δ p95 |
|---|---|---|---|---|---|---|
| cold-broker | bench-a | prepare | 3.4 → 3.5 | +0.2 | 6.1 → 4.6 | -1.4 |
| cold-broker | bench-a | submit→text | 1912.2 → 2113.9 | +201.8 | 3211.9 → 3232.5 | +20.6 |
| cold-broker | bench-a | start→text | 1915.4 → 2117.2 | +201.8 | 3215.9 → 3235.5 | +19.6 |
| cold-broker | bench-a | submit→done | 2386.5 → 2161.2 | -225.3 | 3713.8 → 3263.7 | -450.2 |
| cold-broker | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| cold-broker | bench-a | init | 614.8 → 669.5 | +54.7 | 657.4 → 744.1 | +86.7 |
| cold-broker | bench-a | text wait | 1296.3 → 1439.7 | +143.4 | 2577.8 → 2580.1 | +2.3 |
| cold-broker | bench-a | completion | 501.9 → 31.3 | -470.6 | 620.7 → 47.3 | -573.4 |
| cold-broker | bench-a | tail | 475.8 → 0.1 | -475.7 | 570.8 → 0.1 | -570.6 |
| cold-broker | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | -0.0 |
| warm-send | bench-a | prepare | 0.4 → 0.4 | +0.0 | 0.6 → 1.1 | +0.6 |
| warm-send | bench-a | submit→text | 1854.6 → 2150.2 | +295.6 | 2553.8 → 4306.4 | +1752.6 |
| warm-send | bench-a | start→text | 1855.0 → 2150.6 | +295.6 | 2554.4 → 4306.8 | +1752.4 |
| warm-send | bench-a | submit→done | 2382.5 → 2182.0 | -200.5 | 3001.9 → 4338.5 | +1336.6 |
| warm-send | bench-a | queue wait | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | -0.0 |
| warm-send | bench-a | init | 574.1 → 713.7 | +139.6 | 664.4 → 822.9 | +158.6 |
| warm-send | bench-a | text wait | 1284.2 → 1429.4 | +145.1 | 1956.2 → 3487.1 | +1530.8 |
| warm-send | bench-a | completion | 475.6 → 31.5 | -444.0 | 689.9 → 47.0 | -642.8 |
| warm-send | bench-a | tail | 449.2 → 0.1 | -449.1 | 667.7 → 0.1 | -667.6 |
| warm-send | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | 0.0 |
| warm-send-probe | bench-a | prepare | 0.4 → 0.3 | -0.1 | 0.5 → 0.6 | +0.1 |
| warm-send-probe | bench-a | submit→text | 2110.3 → 2365.6 | +255.3 | 3111.2 → 3531.5 | +420.3 |
| warm-send-probe | bench-a | start→text | 2110.9 → 2365.9 | +255.1 | 3111.8 → 3531.8 | +420.1 |
| warm-send-probe | bench-a | submit→done | 2644.2 → 2397.1 | -247.1 | 3640.7 → 3563.6 | -77.0 |
| warm-send-probe | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| warm-send-probe | bench-a | probe | 210.5 → 261.3 | +50.8 | 246.0 → 290.1 | +44.0 |
| warm-send-probe | bench-a | init | 568.5 → 683.1 | +114.7 | 669.4 → 791.6 | +122.2 |
| warm-send-probe | bench-a | text wait | 1285.6 → 1374.1 | +88.5 | 2366.1 → 2665.3 | +299.2 |
| warm-send-probe | bench-a | completion | 545.1 → 32.0 | -513.1 | 670.3 → 42.4 | -627.8 |
| warm-send-probe | bench-a | tail | 523.2 → 0.1 | -523.1 | 628.4 → 0.1 | -628.3 |
| warm-send-probe | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | +0.0 |
| prepared-send | bench-a | prepare | 0.3 → 0.2 | -0.0 | 1.7 → 0.4 | -1.3 |
| prepared-send | bench-a | submit→text | 1908.6 → 1969.6 | +61.0 | 2098.1 → 2422.4 | +324.3 |
| prepared-send | bench-a | start→text | 1909.0 → 1969.8 | +60.8 | 2098.3 → 2422.6 | +324.3 |
| prepared-send | bench-a | submit→done | 2501.2 → 1997.4 | -503.8 | 2703.2 → 2451.7 | -251.5 |
| prepared-send | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.1 | +0.0 |
| prepared-send | bench-a | init | 630.5 → 696.6 | +66.1 | 729.3 → 752.8 | +23.5 |
| prepared-send | bench-a | text wait | 1298.8 → 1328.9 | +30.1 | 1440.9 → 1683.7 | +242.8 |
| prepared-send | bench-a | completion | 530.2 → 29.4 | -500.8 | 715.9 → 42.6 | -673.3 |
| prepared-send | bench-a | tail | 498.8 → 0.1 | -498.7 | 689.4 → 0.1 | -689.3 |
| prepared-send | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | -0.0 |
| warm-status | bench-a | prepare | 0.3 → 0.4 | +0.1 | 0.8 → 0.5 | -0.3 |
| warm-status | bench-a | submit→done | 239.9 → 263.2 | +23.3 | 274.5 → 379.3 | +104.7 |
| warm-status | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | -0.0 |
| warm-status | bench-a | probe | 239.3 → 262.4 | +23.2 | 274.1 → 378.5 | +104.4 |
| warm-status | bench-a | completion | 0.1 → 0.1 | +0.0 | 0.2 → 0.4 | +0.2 |
| warm-status | bench-a | cleanup | 0.0 → 0.0 | -0.0 | 0.0 → 0.0 | -0.0 |
| resumed-context | bench-a | prepare | 0.4 → 0.4 | +0.1 | 0.6 → 0.9 | +0.3 |
| resumed-context | bench-a | submit→text | 1942.1 → 1790.7 | -151.4 | 2014.4 → 1884.7 | -129.7 |
| resumed-context | bench-a | start→text | 1942.6 → 1791.1 | -151.6 | 2015.0 → 1885.2 | -129.8 |
| resumed-context | bench-a | submit→done | 2459.2 → 2341.3 | -117.9 | 2542.5 → 2438.0 | -104.5 |
| resumed-context | bench-a | queue wait | 0.0 → 0.0 | +0.0 | 0.0 → 0.0 | +0.0 |
| resumed-context | bench-a | init | 587.1 → 675.3 | +88.2 | 649.2 → 792.9 | +143.7 |
| resumed-context | bench-a | text wait | 1359.9 → 1070.8 | -289.1 | 1404.0 → 1209.0 | -195.0 |
| resumed-context | bench-a | completion | 501.2 → 533.5 | +32.3 | 638.7 → 704.8 | +66.1 |
| resumed-context | bench-a | tail | 469.4 → 475.7 | +6.3 | 601.6 → 667.8 | +66.2 |
| resumed-context | bench-a | cleanup | 0.0 → 0.0 | 0.0 | 0.0 → 0.0 | +0.0 |
