"""Half-wave rectifier + filter — a polarity demo for the build guide.

A diode, a polarised (electrolytic) capacitor, and a load resistor: the smallest
circuit that exercises *both* polarity kinds the build guide calls out — a diode
cathode and an electrolytic + terminal — alongside a non-polarised resistor.

    IN ──▶│── OUT ──┬──[ R1 ]── GND
         D1         │
                  [ C1 ]  (electrolytic, + up)
                    │
                   GND

This is a *layout / build-guide* fixture (place → guide), not an analytic one.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob guide`/`lob board` set it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
D_FOOTPRINT = "Diode_SMD:D_SOD-123"
CP_FOOTPRINT = "Capacitor_SMD:CP_Elec_3x5.3"


def build():
    """Construct the rectifier + filter in the default SKiDL circuit."""
    d1 = Part("Device", "D", value="1N4148", footprint=D_FOOTPRINT, ref="D1")
    # Polarised (electrolytic) cap — the CP_Elec footprint is what marks it + in
    # the build guide, regardless of the "C" reference-designator prefix.
    c1 = Part("Device", "C_Polarized", value="10u", footprint=CP_FOOTPRINT, ref="C1")
    r1 = Part("Device", "R", value="10k", footprint=R_FOOTPRINT, ref="R1")

    vin = Net("IN")
    vout = Net("OUT")
    gnd = Net("GND")

    vin += d1[2]  # anode
    vout += d1[1], c1[1], r1[1]  # cathode, cap +, load
    gnd += c1[2], r1[2]  # cap −, load return

    return {"D1": d1, "C1": c1, "R1": r1}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the rectifier demo KiCad netlist.")
    parser.add_argument("--output", "-o", default="rectifier.net")
    args = parser.parse_args(argv)

    build()
    ERC()
    generate_netlist(file_=args.output)
    print(f"wrote netlist: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
