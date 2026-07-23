"""Double-sided board demo — mixed SMD + through-hole (DESIGN 6.1/6.2).

An RC low-pass whose signal parts are SMD but whose I/O is a through-hole pin
header — the mixed construction real Eurorack modules use (SMD/PCBA components on
one side, THT + silkscreen on the other). It exercises the layout pipeline's
side-aware placement: SMD parts and the THT connector land on opposite copper
sides, and the back-side ground pour has to coexist with whatever's mounted on
that side.

    IN ──[ R1 ]──┬── OUT        J1: 1=IN  2=OUT  3=GND  (through-hole header)
                 │
               [ C1 ]
                 │
                GND

R1/C1 are SMD 0805; J1 is a 1x03 2.54 mm through-hole header. This is a *layout*
fixture (place → route → DRC on a two-sided board), not an analytic one.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob board` sets it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

R_VALUE = "1k"
C_VALUE = "159n"

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
C_FOOTPRINT = "Capacitor_SMD:C_0805_2012Metric"
# A through-hole I/O header — mounts on the opposite side from the SMD parts.
J_FOOTPRINT = "Connector_PinHeader_2.54mm:PinHeader_1x03_P2.54mm_Vertical"


def build():
    """Construct the mixed SMD/THT RC low-pass in the default SKiDL circuit."""
    r1 = Part("Device", "R", value=R_VALUE, footprint=R_FOOTPRINT, ref="R1")
    c1 = Part("Device", "C", value=C_VALUE, footprint=C_FOOTPRINT, ref="C1")
    j1 = Part("Connector_Generic", "Conn_01x03", ref="J1", footprint=J_FOOTPRINT)

    vin = Net("IN")
    vout = Net("OUT")
    gnd = Net("GND")

    vin += r1[1], j1[1]
    vout += r1[2], c1[1], j1[2]
    gnd += c1[2], j1[3]

    return {"R1": r1, "C1": c1, "J1": j1}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the double-sided RC demo KiCad netlist.")
    parser.add_argument("--output", "-o", default="double_sided.net")
    args = parser.parse_args(argv)

    build()
    ERC()
    generate_netlist(file_=args.output)
    print(f"wrote netlist: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
