# Puget Audio Board Pipeline — DESIGN.md

Status: DRAFT — table of contents, sections to be filled in.

## Table of Contents

1. **Vision & Scope**
   1.1 Problem statement
   1.2 What "done" looks like
   1.3 Non-goals (v1)

2. **Architecture Overview**
   2.1 Pipeline stages (map to the 8-step loop)
   2.2 Orchestration model (CLI tool? library? workflow engine?)
   2.3 Canonical circuit representation (the IR that DSLs compile to / tools read from)
   2.4 Repo & project structure

3. **Circuit Definition Layer**
   3.1 SKiDL (v1, Python)
   3.2 Path to a native Rust DSL (v2+)
   3.3 DSL-agnostic circuit IR — what it must capture to support multiple frontends
   3.4 Reusable component/subcircuit library (op-amp stages, OTA stages, RIAA networks, etc.)

4. **Validation Layer**
   4.1 ERC (first pass) — what it catches, what it doesn't
   4.2 Project-specific rule extensions (Eurorack power header conventions, panel mount checks)

5. **Simulation Layer**
   5.1 ngspice integration
   5.2 PedalKernel integration — reusing the existing accuracy/harness infrastructure
   5.3 What gets simulated pre-layout vs. what waits

6. **Layout & DFM**
   6.1 KiCad pcbnew integration (from IR/netlist to board file)
   6.2 DFM checks (JLCDFM or equivalent) before ordering
   6.3 Manual layout work still required — where the human loop stays in

7. **Panel Design**
   7.1 DXF output
   7.2 SVG output
   7.3 KiCad-native panel workflow
   7.4 Mechanical fit checks against PCB (jack/pot alignment)

8. **Manufacturing Outputs**
   8.1 Gerbers via KiCad (v1 path)
   8.2 Direct-to-gerber (future — if/when it's worth it)
   8.3 Output validation before submission

9. **BOM & PCBA Pipeline**
   9.1 BOM generation from IR
   9.2 CPL (component placement list) generation
   9.3 JLCPCB API integration (quoting, order creation, tracking)
   9.4 BOM accuracy/verification — this has to be airtight
   9.5 Multi-vendor part sourcing rules (JLCPCB basic vs extended, LCSC fallback)

10. **Thru-Hole BOM & Ordering**
    10.1 Distributor sourcing (LCSC/Mouser/DigiKey)
    10.2 Kitting for in-house assembly at FPL

11. **Part Stock & Inventory Management**
    11.1 Data model (parts, boards, revisions, quantities on hand)
    11.2 Shared-component tracking across SKUs (2OPFM, PHRSR/SCANNER/EG/TVCA, future Winterbloom line)
    11.3 Source of truth — where inventory state actually lives

12. **Reorder Automation**
    12.1 Trigger logic (thresholds, lead-time awareness)
    12.2 n8n integration
    12.3 Human-in-the-loop vs. fully automatic ordering

13. **QC, Test & Revision Control**
    13.1 Bench test / functional test procedure before ship
    13.2 Board revision scheme (silkscreen rev ↔ git tag ↔ IR version)
    13.3 Traceability (which rev shipped to which retailer/order)

14. **Roadmap & Milestones**
    14.1 Phase 0 — slew limiter as end-to-end proof of concept
    14.2 Phase 1 — MVP covering must-have outputs (KiCad/PedalKernel/SPICE)
    14.3 Phase 2 — panel outputs (DXF/SVG/KiCad)
    14.4 Phase 3 — BOM/PCBA hardening + inventory/reorder automation
    14.5 Phase 4 — direct-to-gerber, additional DSLs (stretch)

15. **Open Questions**
    (running list — filled in as we go)

---

## 1. Vision & Scope

*(to be filled in)*
