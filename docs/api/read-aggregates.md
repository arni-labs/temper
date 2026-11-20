# Counting records by value

A facet read counts the records a list read would show, grouped by the value of
one property (ADR-0188):

```
GET /tdata/Cards?$apply=groupby((kind),aggregate($count as count))
```

```json
{
  "@odata.context": "$metadata#Cards",
  "value": [
    {"kind": "find", "count": 1204},
    {"kind": "steer", "count": 1189},
    {"kind": null, "count": 7}
  ],
  "@odata.nextLink": "Cards?$apply=...&$skiptoken=..."
}
```

- The property is `Status` or a scalar property the entity type declares.
- The count's name is yours: `aggregate($count as n)` answers `"n": 1204`. It
  cannot be the property's own name.
- Records without a value, or with a JSON null, are one group whose value is
  null.
- A number group is written as an integer when it is one (`3`, whether a record
  holds `3` or `3.0`), else as a decimal.

## Options

| Option | Meaning |
| --- | --- |
| `$filter` | Which records are counted, as in a list read. |
| `$orderby` | `<property> asc` (the default), or `count desc` (largest first, ties by value), or `count desc,<property> asc`. Use the count's name for `count`. |
| `$top` | Groups per page; the default and maximum are a list read's. |
| `$skiptoken` | From `@odata.nextLink`. Follow the link as given. |

Groups sort by value: numbers by size, strings by their bytes (`B` before `a`),
the null group last. A property whose records hold values of several JSON types
sorts booleans, then numbers, then strings, then anything else.

## What a reader sees

A record the reader may not read is never counted, so no group or count says
anything about it. Under a policy that judges each record, the read walks every
matching record to count it: it answers, but it takes time in proportion to the
records it judges (about 4,350 a second on the machine ADR-0188 measured).

When every record is allowed, the filter is one the store evaluates exactly,
and the property is `Status` or a declared, non-nullable number or string with
no default, the store counts the groups in one statement.

## Refused

These answer `400`:

- any other `$apply` (another aggregate, two properties, `filter(...)`, ...);
- `$apply` with `$expand`, `$select`, `$count`, `$skip`, `$search` or
  `actions=true`;
- any other `$orderby`;
- a property the entity type does not declare;
- a `$skiptoken` that is not this facet's.
