#!/usr/bin/env python3
"""Generate the larger example problems from a fixed random seed.

Usage (from the repository root):
    python3 tools/generate_examples.py

Writes examples/supply_chain_lp.json and examples/facility_location_milp.json.
The output is deterministic; rerunning it rewrites identical files.
"""

import json
import random

SCHEMA = "../schema/problem.schema.json"


def table(sets, fn, *prefix):
    """Nested parameter table over the given element lists."""
    if not sets:
        return fn(*prefix)
    return {str(e): table(sets[1:], fn, *prefix, e) for e in sets[0]}


def write(name, problem):
    with open(f"examples/{name}.json", "w") as f:
        json.dump(problem, f, indent=2)
        f.write("\n")


def supply_chain_lp():
    """Multi-plant, multi-product, multi-period production, inventory and shipping.

    Seasonal demand peaks above plant capacity, so the optimum builds
    inventory ahead of the peak and leaves some demand unmet. Challenging for
    a simplex code because it is highly degenerate (many
    transportation-style constraints), mixes equalities with inequalities and
    upper bounds, links periods through inventory, and uses a conditional
    forall to split the first period's balance from the rest.
    """
    rng = random.Random(20261103)
    plants = [f"P{i}" for i in range(1, 6)]
    markets = [f"M{i:02d}" for i in range(1, 13)]
    products = ["alpha", "beta", "gamma", "delta"]
    periods = {"from": 1, "to": 6}
    T = range(1, 7)
    hours = {p: rng.choice([1.0, 1.5, 2.0, 2.5]) for p in products}
    season = {1: 0.6, 2: 0.8, 3: 1.0, 4: 1.5, 5: 1.7, 6: 0.9}  # peak demand exceeds capacity
    return {
        "$schema": SCHEMA,
        "version": "1.0",
        "sets": {"PLANTS": plants, "MARKETS": markets, "PRODUCTS": products, "PERIODS": periods},
        "parameters": {
            "hours": {"index": ["PRODUCTS"], "values": table([products], lambda p: hours[p])},
            "plant_hours": {"index": ["PLANTS"], "values": table([plants], lambda f: rng.randint(250, 310))},
            "make_cost": {"index": ["PLANTS", "PRODUCTS"],
                          "values": table([plants, products], lambda f, p: round(rng.uniform(8, 20), 2))},
            "ship_cost": {"index": ["PLANTS", "MARKETS"],
                          "values": table([plants, markets], lambda f, m: round(rng.uniform(1, 9), 2))},
            "lane_cap": {"index": ["PLANTS", "MARKETS"],
                         "values": table([plants, markets], lambda f, m: rng.choice([60, 80, 100, 120]))},
            "demand": {"index": ["MARKETS", "PRODUCTS", "PERIODS"],
                       "values": table([markets, products, T], lambda m, p, t: round(rng.randint(5, 35) * season[t]))},
            "hold_cost": {"index": ["PRODUCTS"], "values": table([products], lambda p: round(rng.uniform(0.5, 2), 2))},
            "storage": {"index": ["PLANTS"], "values": table([plants], lambda f: rng.choice([150, 200, 250]))},
            "initial_stock": {"index": ["PLANTS", "PRODUCTS"],
                              "values": table([plants, products], lambda f, p: rng.randint(0, 30))},
            "shortage_penalty": 75,
        },
        "variables": [
            {"name": "make", "type": "continuous", "index": ["PLANTS", "PRODUCTS", "PERIODS"], "lower": 0},
            {"name": "ship", "type": "continuous", "index": ["PLANTS", "MARKETS", "PRODUCTS", "PERIODS"], "lower": 0},
            {"name": "stock", "type": "continuous", "index": ["PLANTS", "PRODUCTS", "PERIODS"], "lower": 0},
            {"name": "short", "type": "continuous", "index": ["MARKETS", "PRODUCTS", "PERIODS"], "lower": 0,
             "description": "Unmet demand, penalized"},
        ],
        "constraints": [
            {"name": "plant_capacity", "forall": "f in PLANTS, t in PERIODS",
             "expr": "sum(p in PRODUCTS) hours[p] * make[f,p,t] <= plant_hours[f]"},
            {"name": "first_balance", "forall": "f in PLANTS, p in PRODUCTS, t in PERIODS: t == 1",
             "expr": "stock[f,p,t] == initial_stock[f,p] + make[f,p,t] - sum(m in MARKETS) ship[f,m,p,t]"},
            {"name": "balance", "forall": "f in PLANTS, p in PRODUCTS, t in PERIODS: t > 1",
             "expr": "stock[f,p,t] == stock[f,p,t-1] + make[f,p,t] - sum(m in MARKETS) ship[f,m,p,t]"},
            {"name": "storage_limit", "forall": "f in PLANTS, t in PERIODS",
             "expr": "sum(p in PRODUCTS) stock[f,p,t] <= storage[f]"},
            {"name": "meet_demand", "forall": "m in MARKETS, p in PRODUCTS, t in PERIODS",
             "expr": "sum(f in PLANTS) ship[f,m,p,t] + short[m,p,t] == demand[m,p,t]"},
            {"name": "lane", "forall": "f in PLANTS, m in MARKETS, t in PERIODS",
             "expr": "sum(p in PRODUCTS) ship[f,m,p,t] <= lane_cap[f,m]"},
        ],
        "solve": {
            "method": "minimize",
            "objective": "sum(f in PLANTS, p in PRODUCTS, t in PERIODS) (make_cost[f,p] * make[f,p,t] + hold_cost[p] * stock[f,p,t])"
                         " + sum(f in PLANTS, m in MARKETS, p in PRODUCTS, t in PERIODS) ship_cost[f,m] * ship[f,m,p,t]"
                         " + sum(m in MARKETS, p in PRODUCTS, t in PERIODS) shortage_penalty * short[m,p,t]",
            "solutions": "one",
        },
    }


def facility_location_milp():
    """Capacitated facility location with modular capacity.

    Binary open decisions, general-integer capacity modules (0..4), and
    continuous flows. Fixed costs dominate, so the LP relaxation is weak and
    branch and bound has real work to do.
    """
    rng = random.Random(4242)
    facilities = [f"F{i:02d}" for i in range(1, 19)]
    customers = [f"C{i:02d}" for i in range(1, 51)]
    pos = {k: (rng.uniform(0, 100), rng.uniform(0, 100)) for k in facilities + customers}

    def dist(a, b):
        (x1, y1), (x2, y2) = pos[a], pos[b]
        return round(((x1 - x2) ** 2 + (y1 - y2) ** 2) ** 0.5 / 10, 2)

    return {
        "$schema": SCHEMA,
        "version": "1.0",
        "sets": {"FACILITIES": facilities, "CUSTOMERS": customers},
        "parameters": {
            "open_cost": {"index": ["FACILITIES"], "values": table([facilities], lambda f: rng.randint(400, 900))},
            "module_cost": {"index": ["FACILITIES"], "values": table([facilities], lambda f: rng.randint(150, 300))},
            "module_size": 60,
            "demand": {"index": ["CUSTOMERS"], "values": table([customers], lambda c: rng.randint(5, 35))},
            "unit_cost": {"index": ["FACILITIES", "CUSTOMERS"], "values": table([facilities, customers], dist)},
        },
        "variables": [
            {"name": "open", "type": "binary", "index": ["FACILITIES"]},
            {"name": "modules", "type": "integer", "index": ["FACILITIES"], "lower": 0, "upper": 4},
            {"name": "serve", "type": "continuous", "index": ["FACILITIES", "CUSTOMERS"], "lower": 0},
        ],
        "constraints": [
            {"name": "meet_demand", "forall": "c in CUSTOMERS",
             "expr": "sum(f in FACILITIES) serve[f,c] == demand[c]"},
            {"name": "capacity", "forall": "f in FACILITIES",
             "expr": "sum(c in CUSTOMERS) serve[f,c] <= module_size * modules[f]"},
            {"name": "modules_need_open", "forall": "f in FACILITIES",
             "expr": "modules[f] <= 4 * open[f]"},
            {"name": "serve_need_open", "forall": "f in FACILITIES, c in CUSTOMERS",
             "expr": "serve[f,c] <= demand[c] * open[f]"},
        ],
        "solve": {
            "method": "minimize",
            "objective": "sum(f in FACILITIES) (open_cost[f] * open[f] + module_cost[f] * modules[f])"
                         " + sum(f in FACILITIES, c in CUSTOMERS) unit_cost[f,c] * serve[f,c]",
            "solutions": "one",
        },
    }


if __name__ == "__main__":
    write("supply_chain_lp", supply_chain_lp())
    write("facility_location_milp", facility_location_milp())
