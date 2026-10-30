# ShipShape demo service (proof-suite scratch tree)

Small order-pricing service used by the Forge 2.0 proof suite
(FRG-29). Deliberately ordinary code with a stated contract.

## Contract

- `AGENTS.md` is the entry file. `docs/db-conventions.md` is the
  routed database contract — it governs all database reads.
- Money values are formatted with `src/helpers.py::format_money`.
  Do not reimplement formatting locally.
