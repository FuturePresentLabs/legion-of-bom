"""Two-stage RC low-pass ladder — a routing/layout demo circuit.

Same passive-filter family as ``rc_lowpass.py``, but with four parts and an
extra internal node — enough to exercise the layout pipeline's router on a
congested board (the naive grid placer lines all four parts up in a row, so the
signal nets have to weave past several pads, and the router may drop to the back
copper with vias to get across).

Topology::

    IN ──[ R1 ]──┬── MID ──[ R2 ]──┬── OUT
                 │                  │
               [ C1 ]            [ C2 ]
                 │                  │
                GND                GND

Two cascaded RC sections. The analytic behaviour is a two-pole roll-off; this
fixture exists for the *layout* pipeline (place -> route -> DRC), not as a
precise filter-response reference, so the exact values are illustrative.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run`/`lob board` set it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

R1_VALUE = "1k"
R2_VALUE = "1k"
C1_VALUE = "159n"
C2_VALUE = "159n"

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"


def build():
    """Construct the two-stage RC ladder in the default SKiDL circuit."""
    r1 = Part("Device", "R", value=R1_VALUE, footprint=R_FOOTPRINT, ref="R1")
    r2 = Part("Device", "R", value=R2_VALUE, footprint=R_FOOTPRINT, ref="R2")
    c1 = Part("Device", "C", value=C1_VALUE, footprint=C_FOOTPRINT, ref="C1")
    c2 = Part("Device", "C", value=C2_VALUE, footprint=C_FOOTPRINT, ref="C2")

    vin = Net("IN")
    mid = Net("MID")
    vout = Net("OUT")
    gnd = Net("GND")

    vin += r1[1]
    mid += r1[2], c1[1], r2[1]
    vout += r2[2], c2[1]
    gnd += c1[2], c2[2]

    return {"R1": r1, "R2": r2, "C1": c1, "C2": c2}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the two-stage RC ladder KiCad netlist.")
    parser.add_argument("--output", "-o", default="rc_ladder.net")
    args = parser.parse_args(argv)

    build()
    ERC()
    generate_netlist(file_=args.output)
    print(f"wrote netlist: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
