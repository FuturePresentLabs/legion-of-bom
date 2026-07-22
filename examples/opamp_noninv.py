"""Non-inverting op-amp gain stage — Phase 0 demo circuit #2.

A textbook non-inverting amplifier. Exercises the same pipeline as the RC filter
with a different topology (an active device) and a different analytic check:

    closed-loop gain  A = 1 + Rf/Rg

Topology::

    IN ───────────────(+)\\
                          >──┬─── OUT
              ┌──────────(-)/   │
              │               [ Rf ]   (feedback: OUT → FB)
             FB ──────────────┘ │
              │                 │
            [ Rg ]              │
              │                 │
             GND            (OUT probed)

The op-amp is KiCad's ideal ``Simulation_SPICE:OPAMP`` symbol — an ideal
simulation part, NOT a real silicon device. Its pin roles (1=+, 2=−, 5=output,
3/4=rails) are read from the KiCad symbol library, never assumed from memory.
The simulation stage models it as an ideal VCVS, so the sim reproduces the ideal
gain formula the verify stage checks against.

With Rf = 9 kΩ and Rg = 1 kΩ:

    A = 1 + 9k/1k = 10  →  20.00 dB

Rf/Rg are identified topologically (feedback resistor touches OUT, ground
resistor touches GND), so the check doesn't depend on reference-designator names.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run` sets this up — see docs/TOOLING.md.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

RF_VALUE = "9k"  # feedback resistor (OUT → FB)
RG_VALUE = "1k"  # ground resistor (FB → GND)

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"


def build():
    """Construct the non-inverting amplifier in the default SKiDL circuit."""
    # value is cosmetic here (ideal sim part); the SPICE model is resolved from
    # the symbol's Sim.* fields, not the value. A real op-amp's MPN would come
    # from the parts library.
    opamp = Part("Simulation_SPICE", "OPAMP", value="OPAMP (ideal)", ref="U1")
    rf = Part("Device", "R", value=RF_VALUE, footprint=R_FOOTPRINT, ref="R1")
    rg = Part("Device", "R", value=RG_VALUE, footprint=R_FOOTPRINT, ref="R2")

    vin = Net("IN")
    vout = Net("OUT")
    fb = Net("FB")
    gnd = Net("GND")
    vcc = Net("VCC")
    vee = Net("VEE")

    vin += opamp[1]  # non-inverting input (+)
    fb += opamp[2], rf[2], rg[1]  # inverting input (−), feedback node
    vout += opamp[5], rf[1]  # output, feedback resistor high side
    gnd += rg[2]
    vcc += opamp[3]  # V+ rail (ignored by the ideal-VCVS sim model)
    vee += opamp[4]  # V− rail

    return {"U1": opamp, "R1": rf, "R2": rg}


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit the non-inverting amp KiCad netlist.")
    parser.add_argument("--output", "-o", default="opamp_noninv.net")
    args = parser.parse_args(argv)

    build()
    ERC()
    generate_netlist(file_=args.output)
    print(f"wrote netlist: {args.output}", file=sys.stderr)


if __name__ == "__main__":
    main()
