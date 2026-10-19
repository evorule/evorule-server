---
name: evorule-deterministic-compute
description: Route deterministic computation to an evorule server for exact, verifiable, bit-replayable results — precise arithmetic and decimals, classical mechanics, GPS/relativity clock corrections, fluid dynamics, quantum energy levels, measurement uncertainty (GUM), finance and amortization, checksums (Luhn/ISBN/IBAN), RMB uppercase amounts, unit/dimension conversion, technical indicators. Use whenever a computation must be exact, auditable, or independently re-computable — or when the user asks to verify, recompute, or double-check a number. Do NOT use for open-ended semantic tasks (writing, reasoning, classification) — those stay with the model.
---

# evorule Deterministic Compute

An evorule server exposes native computation services that return
**deterministic envelopes**: same input → same output, bit for bit,
with exact rational arithmetic (no floating point). You decide *when*
a computation matters; the server guarantees *how correctly* it is
computed. Every result carries `status: determined` and
`determinism: bitwise_replayable`.

## 1. When to route here

- The result must be **exact** (money, deadlines, physics, measurement)
- The result must be **verifiable** later (audit, compliance, disputes)
- The user asks to **recompute / verify / double-check** a number
- The computation is a known closed-form formula (see families below)

**Do not** route open-ended semantics (drafting, summarizing, judgment
calls) — the server computes, it does not interpret.

**Never simulate arithmetic in-model** (mental math, repeated addition,
back-of-envelope ratios) when a service covers it — model-computed
numbers are not replayable and break the audit chain. If the catalog
has no matching service, say so and compute nothing silently.

## 2. Discover the catalog (do this first, it never goes stale)

```
GET {EVORULE_BASE}/api/services
```

Returns `{"list": [{name, source, version, description, plugin,
sensitive, parameters?}, ...]}` — one entry per service. The
`parameters` field is the JSON Schema contract for that service's
arguments: **read it before invoking an unfamiliar service**.

`EVORULE_BASE` defaults to `http://127.0.0.1:18080` (override with the
environment variable of the same name).

`GET {EVORULE_BASE}/api/health` additionally shows per-family
grouping and deployment state.

## 3. Invoke

```
POST {EVORULE_BASE}/api/services/{name}/invoke
Content-Type: application/json

<args object matching the service's parameters contract>
```

Numerics are **strings** (decimal literals like `"19.99"` or exact
fractions like `"7992739871/1000000000"` — never JSON numbers, never
floating point). Where the contract requires `scale` (decimal places)
and `rounding` (`half_even`/`half_up`/`floor`/`trunc`), both are
mandatory — the server never rounds implicitly.

## 4. Interpret the envelope

Every success carries:

- `status: "determined"` and `determinism: "bitwise_replayable"`
- Result fields per contract (typically a `value` object with
  `unscaled`/`scale`/`decimal_string` plus an `*_exact` reduced
  fraction string)

Failures are **fail-visible with self-diagnosis**: a human-readable
message names the violated constraint (missing parameter, sign
violation, degenerate case like division by zero, consistency-check
mismatch with both values quoted). Surface these messages verbatim —
they are written for the end user. An `undetermined` exit is an honest
capability boundary (e.g. huge-angle sine), not a malfunction.

Marked-sensitive services reject direct invocation (HTTP 403) by
design — they must go through an audited session.

## 5. Worked examples (all values are live-server goldens)

Exact rational with variable bindings (numeric family; the fraction is
authoritative, `decimal_approx` is a 12-digit convenience view):

```json
{"service":"numeric_rational_eval","args":{"expr":"a*b+c","vars":{"a":"3/2","b":"4","c":"1"}}}
→ numerator "7", denominator "1" (exact 7/1), decimal_approx "7.000000000000"
```

GPS satellite clock: eccentricity-period correction at its peak
(e=0.01, a=26 560 km, E≈π/2) ≈ **−22.9 ns**:

```json
{"service":"gnss_eccentricity_correction","args":{"e":"0.01","a":"26560000","E":"1.5707963267948966192311","scale":2,"rounding":"half_even"}}
→ dt_r_ns "-22.90"
```

Relativity, verified against the textbook: Mercury perihelion
precession over 415 orbits/century ≈ **42.96″** (the classic 43″):

```json
{"service":"grav_perihelion_precession","args":{"m":"1988470000000000000000000000000","a":"57909000000","e":"0.2056","g":"66743/1000000000000000","orbits":415,"scale":2,"rounding":"half_even"}}
→ arcsec_total "42.96"
```

Without the correction a GPS clock drifts ~11.5 km/day of ranging
error: `gnss_range_error` with `time_seconds:"38575/1000000000"` →
`distance_m` "11564.494067".

Quantum: electron in a 1 nm infinite well, E₁ ≈ **0.376 eV**:

```json
{"service":"quantum_infinite_well","args":{"n":1,"m":"91093837015/100000000000000000000000000000000000000000","l":"1/1000000000","scale":3,"rounding":"half_even"}}
→ E_ev "0.376"
```

Measurement (GUM): y = x·z with x = 2±0.01, z = 100±1 →
u_y/y = √(0.005²+0.01²) ≈ **1.12 %**:

```json
{"service":"metrology_uncertainty_propagate","args":{"terms":[{"x":"2","u":"0.01","a":1},{"x":"100","u":"1","a":1}],"scale":4,"rounding":"half_even"}}
→ y "200.0000", rel_u "0.0112", u_y "2.2400"
```

Bank-card check digit: payload 7992739871 → Luhn check digit **3**:

```json
{"service":"checksum_luhn","args":{"mode":"append","value":"7992739871"}}
→ check_digit 3, full "79927398713"
```

RMB amount in bill-standard Chinese uppercase:

```json
{"service":"cny_amount_upper","args":{"amount":"107000.53","currency_prefix":true}}
→ upper "人民币壹拾万柒仟元伍角叁分"
```

Classical mechanics: F = m·a with m=2, a=3 → **6 N** (`mechanics_newton_second`,
solve:"f"); fluid: Bernoulli pressure drop on 2→4 m/s acceleration →
`fluid_bernoulli` p₂ = **194000 Pa**.

## 6. Family quick reference

| Family | What it computes |
|---|---|
| numeric | exact rational/decimal arithmetic, number theory, interest/amortization, unit & dimension conversion, vars bindings |
| mechanics | F=ma, kinematics, momentum/impulse, energy, Ohm's law, resistor networks, discrete PID step |
| gnss | GPS clock: factory pre-bias, broadcast polynomial, eccentricity correction, relativistic drift, total synthesis, range error |
| grav | Schwarzschild radius, perihelion precession, light deflection |
| fluid | Bernoulli, continuity, hydrostatic pressure |
| quantum | infinite-well and harmonic-oscillator energy levels (J and eV) |
| metrology | GUM uncertainty propagation (multiplicative power models) |
| checksum | Luhn, ISBN-10/13, IBAN mod-97 (validate + check-digit) |
| cny | RMB amounts in bill-standard Chinese uppercase |
| indicator | SMA/EMA/MACD/RSI technical indicators |
| physics | rigid-body simulation, energy, gravitational band |
| inference | SI dimension extraction/comparison/verification, rule-inference helpers |
| geometry | collision/clearance/occlusion checks |
| template | deterministic template rendering |
| browser | browser automation services |
| demo | showcase services |

The live catalog (`GET /api/services`) is authoritative — new services
appear there without this file needing an update.

## 7. MCP transport

Prefer MCP? A zero-dependency adapter exposing two tools
(`list_services`, `invoke_service`) lives in `mcp/evorule-mcp.mjs` in
the evorule-server repository — see `mcp/README.md` for client
configuration.
