pub(crate) const PLAN_V2_LOOKUP_EXAMPLES: &str = r#"
# Example: Entity detail lookup
User: Show offshore substation OSS-003 details.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Lookup a single entity by identifier."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreSubstation",
      "fields": ["name", "shortName", "sapLocationId", "partOfOffshoreWindFarmUid"],
      "filter": { "shortName": { "eq": "OSS-003" } },
      "order": null,
      "first": 20,
      "offset": null
    }
  ]
}

# Example: Scoped list query
User: List turbines with shortName containing T1.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Use post-fetch filtering when the filter operator is better expressed in typed plan steps."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": ["name", "shortName", "sapLocationId"],
      "filter": null,
      "order": null,
      "first": 200,
      "offset": null
    },
    {
      "id": "s2",
      "op": "filter_rows",
      "source": "s1",
      "field": "shortName",
      "operator": "contains",
      "value": "T1"
    }
  ]
}
"#;

pub(crate) const PLAN_V2_ANALYTIC_EXAMPLES: &str = r#"
# Example: Count by group
User: Count turbines by stringName.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Fetch the grouping field, then aggregate by it."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": ["stringName"],
      "filter": null,
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s2",
      "op": "aggregate",
      "source": "s1",
      "group_by": ["stringName"],
      "metrics": [{ "op": "count" }]
    }
  ]
}

# Example: Total count
User: How many turbines are there?
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Fetch rows, then aggregate a total count."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": ["name"],
      "filter": null,
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s2",
      "op": "aggregate",
      "source": "s1",
      "group_by": [],
      "metrics": [{ "op": "count" }]
    }
  ]
}

# Example: Rank after aggregation
User: Top 5 turbines by accumulatedDowntime.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Fetch the ranking field, then rank rows by it."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": ["name", "shortName", "accumulatedDowntime"],
      "filter": null,
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s2",
      "op": "rank",
      "source": "s1",
      "by": "accumulatedDowntime",
      "direction": "desc",
      "limit": 5
    }
  ]
}

# Example: Ascending rank
User: Bottom 3 turbines by accumulatedDowntime.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Fetch the ranking field, then rank rows ascending for a bottom-N request."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": ["name", "shortName", "accumulatedDowntime"],
      "filter": null,
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s2",
      "op": "rank",
      "source": "s1",
      "by": "accumulatedDowntime",
      "direction": "asc",
      "limit": 3
    }
  ]
}

# Example: Compare two scoped subsets
User: Compare tag counts between categoryDescription Weather and Electrical for plantId PLANT-005.
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": ["Use two fetches with explicit scoped filters, aggregate each, then compare."],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryTag",
      "fields": ["id"],
      "filter": {
        "categoryDescription": { "eq": "Weather" },
        "plantId": { "eq": "PLANT-005" }
      },
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s2",
      "op": "aggregate",
      "source": "s1",
      "group_by": [],
      "metrics": [{ "op": "count" }]
    },
    {
      "id": "s3",
      "op": "fetch",
      "root_field": "queryTag",
      "fields": ["id"],
      "filter": {
        "categoryDescription": { "eq": "Electrical" },
        "plantId": { "eq": "PLANT-005" }
      },
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s4",
      "op": "aggregate",
      "source": "s3",
      "group_by": [],
      "metrics": [{ "op": "count" }]
    },
    {
      "id": "s5",
      "op": "compare",
      "left": "s2",
      "right": "s4",
      "metric": { "op": "count" }
    }
  ]
}

# Example: Compare farm-scoped turbine averages via parent relation
User: Compare average accumulatedDowntime between Wind Farm 1 and Wind Farm 2.
PlanV2:
{
  "version": "v2",
  "rewrites": ["parent_relation_rewrite"],
  "notes": [
    "Fetch each requested wind farm and project turbine downtime through the farm-to-turbine relation.",
    "Aggregate each farm's turbine downtimes, then compare the averages.",
    "Do not guess farm membership by binding plantId into a turbine UID filter."
  ],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryOffshoreWindFarm",
      "fields": ["name", "hasOffshoreWindTurbine.accumulatedDowntime"],
      "filter": { "name": { "eq": "Wind Farm 1" } },
      "order": null,
      "first": 20,
      "offset": null
    },
    {
      "id": "s2",
      "op": "fetch",
      "root_field": "queryOffshoreWindFarm",
      "fields": ["name", "hasOffshoreWindTurbine.accumulatedDowntime"],
      "filter": { "name": { "eq": "Wind Farm 2" } },
      "order": null,
      "first": 20,
      "offset": null
    },
    {
      "id": "s3",
      "op": "aggregate",
      "source": "s1",
      "group_by": [],
      "metrics": [{ "op": "avg", "field": "hasOffshoreWindTurbine.accumulatedDowntime" }]
    },
    {
      "id": "s4",
      "op": "aggregate",
      "source": "s2",
      "group_by": [],
      "metrics": [{ "op": "avg", "field": "hasOffshoreWindTurbine.accumulatedDowntime" }]
    },
    {
      "id": "s5",
      "op": "compare",
      "left": "s3",
      "right": "s4",
      "metric": { "op": "avg", "field": "hasOffshoreWindTurbine.accumulatedDowntime" }
    }
  ]
}
"#;

pub(crate) const PLAN_V2_DISTANCE_EXAMPLES: &str = r#"
# Example: Vessel-to-turbine distance
User: What is the closest turbine to "the wagon"?
PlanV2:
{
  "version": "v2",
  "rewrites": [],
  "notes": [
    "Fetch the vessel identity by exact name.",
    "Fetch the latest AIS position for that vessel by mmsi.",
    "Fetch turbine coordinate leaf fields.",
    "Compute haversine distance and rank ascending."
  ],
  "steps": [
    {
      "id": "s1",
      "op": "fetch",
      "root_field": "queryVessel",
      "fields": ["name", "mmsi"],
      "filter": { "name": { "eq": "the wagon" } },
      "order": null,
      "first": 20,
      "offset": null
    },
    {
      "id": "s2",
      "op": "fetch",
      "root_field": "queryHistoricalAisVesselpos",
      "fields": ["mmsi", "lat", "lon", "messageTimestamp"],
      "filter": { "mmsi": { "eq": "${s1.mmsi}" } },
      "order": { "desc": "messageTimestamp" },
      "first": 1,
      "offset": null
    },
    {
      "id": "s3",
      "op": "fetch",
      "root_field": "queryOffshoreWindTurbine",
      "fields": [
        "name",
        "shortName",
        "location.point.latitude",
        "location.point.longitude"
      ],
      "filter": null,
      "order": null,
      "first": 2000,
      "offset": null
    },
    {
      "id": "s4",
      "op": "distance_haversine",
      "vessels_source": "s2",
      "target_source": "s3"
    },
    {
      "id": "s5",
      "op": "rank",
      "source": "s4",
      "by": "distanceKm",
      "direction": "asc",
      "limit": 1
    }
  ]
}
"#;

pub(crate) fn planner_examples_for_message(_user_message: &str) -> String {
    format!("{PLAN_V2_LOOKUP_EXAMPLES}\n{PLAN_V2_ANALYTIC_EXAMPLES}\n{PLAN_V2_DISTANCE_EXAMPLES}")
}
