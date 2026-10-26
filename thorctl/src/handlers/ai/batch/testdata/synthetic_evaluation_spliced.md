# Synthetic evaluation fixture

A hand-crafted Scoreboard exercising branches the real docs miss: a partial
run, every delta sign, the progression label collision flip, a family name
over 20 characters, comma-grouped costs, and the exact 50% / 100% rate bins.

<!-- eval-figures:begin -->
<p align="center">
    <img width="800" src="./figures/progression.svg" alt="Aggregate progression: replication rate 75.0% (run 1) to 57.8% (run 5); total cost per run $2,135 to $451">
</p>

| Run | Families | Rate | Δ (pp) | Cost |
|-----|----------|------|--------|------|
| 1 | 2 | 75.0% | — | $2,134.56 |
| 2 | 2 | 85.0% | +10.0 | $300.25 |
| 3 | 2 | 15.0% | −70.0 | $210.00 |
| 4 | 2 | 15.0% | 0.0 | $250.50 |
| 5 | 3 | 57.8% | — | $450.75 |

<p align="center">
    <img width="800" src="./figures/per_family_heatmap.svg" alt="Heatmap of replication rate for 3 families across 6 runs, rows alphabetical by family">
</p>
<!-- eval-figures:end -->

## Scoreboard

| Run | Family | Static GT | Replicated | Partial | Missed | Wrong | Rate | Tokens | Cost | Novel | CISA rate |
|-----|--------|-----------|------------|---------|--------|-------|------|--------|------|-------|-----------|
| 1 | [Alpha](./families/alpha.md) | 10 | 5 | 0 | 5 | 0 | 50% | 1,000,000 (900,000 / 100,000) | $1,234.56 | — | — |
| 1 | [Extraordinarily Long Family Name](./families/long.md) | 10 | 10 | 0 | 0 | 0 | 100.0% | 2,000,000 (1,800,000 / 200,000) | $900.00 | — | 100.0% |
| 2 | [Alpha](./families/alpha.md) | 10 | 7 | 0 | 3 | 1 | 70.0% | 1,100,000 (990,000 / 110,000) | $150.00 | — | — |
| 2 | [Extraordinarily Long Family Name](./families/long.md) | 10 | 10 | 0 | 0 | 0 | 100.0% | 2,100,000 (1,890,000 / 210,000) | $150.25 | — | — |
| 3 | [Alpha](./families/alpha.md) | 10 | 2 | 0 | 8 | 0 | 20.0% | 1,200,000 (1,080,000 / 120,000) | $100.00 | — | — |
| 3 | [Extraordinarily Long Family Name](./families/long.md) | 10 | 1 | 0 | 9 | 2 | 10.0% | 2,200,000 (1,980,000 / 220,000) | $110.00 | — | — |
| 4 | [Alpha](./families/alpha.md) | 10 | 1 | 2 | 7 | 0 | 20.0% | 1,300,000 (1,170,000 / 130,000) | $120.50 | — | — |
| 4 | [Extraordinarily Long Family Name](./families/long.md) | 10 | 1 | 0 | 9 | 1 | 10.0% | 2,300,000 (2,070,000 / 230,000) | $130.00 | — | — |
| 5 | [Alpha](./families/alpha.md) | 10 | 5 | 1 | 4 | 0 | 55.0% | 1,400,000 (1,260,000 / 140,000) | $140.00 | 3 | — |
| 5 | [Extraordinarily Long Family Name](./families/long.md) | 10 | 9 | 0 | 1 | 0 | 90.0% | 2,400,000 (2,160,000 / 240,000) | $150.00 | 5 | — |
| 5 | [Zed](./families/zed.md) | 12 | 3 | 2 | 7 | 1 | 33.3% | 3,000,000 (2,700,000 / 300,000) | $160.75 | 12 | 33.3% |
| 6 | [Alpha](./families/alpha.md) | 10 | 8 | 0 | 2 | 0 | 80.0% | 1,500,000 (1,350,000 / 150,000) | $99.99 | 4 | — |

Trailing prose after the table ends the Scoreboard section.
