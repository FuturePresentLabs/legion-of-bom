"""Non-inverting op-amp gain stage — Phase 0 demo circuit #2.

A textbook non-inverting amplifier built around a **real** op-amp (a TL072 — the
jellybean dual JFET op-amp used all over Eurorack), so the whole pipeline runs on
a buildable circuit: it simulates, verifies, prices a BOM, *and* lays out + routes
to a manufacturable board.

    closed-loop gain  A = 1 + Rf/Rg

Topology (unit A of the TL072)::

    IN ───────────────(+)\\
                          >──┬─── OUT
              ┌──────────(-)/   │
              │               [ Rf ]   (feedback: OUT → FB)
             FB ──────────────┘ │
              │                 │
            [ Rg ]              │
              │                 │
             GND            (OUT probed)

With Rf = 9 kΩ and Rg = 1 kΩ:  A = 1 + 9k/1k = 10 → 20.00 dB.

**How the op-amp carries its own model.** A real device is not an ideal symbol:
the TL072 has a real pinout, a real footprint, and an MPN. But it still needs a
SPICE model to simulate. Rather than special-case op-amps in the generator, the
model travels *with the part* — declared here as `Sim.*` fields (which SKiDL
passes through to the netlist) and resolved by the core `symbols` module. Today
that model is KiCad's built-in ideal op-amp subckt mapped onto the TL072's real
pins; a manufacturer macro-model replaces it when the parts library sources one.

**Pinout is read from the symbol, never memory.** The TL072 pin roles below
(1=out, 2=in−, 3=in+, 4=V−, 5=in+(B), 6=in−(B), 7=out(B), 8=V+) come from KiCad's
`Amplifier_Operational:TL072` symbol (which extends `LM2904`). The unused second
op-amp is terminated as a grounded unity follower so its inputs don't float.

Rf/Rg are identified topologically in the verify stage (feedback resistor touches
OUT, ground resistor touches GND), so the check doesn't depend on the op-amp or
on reference-designator names.

Run standalone (needs KICAD9_SYMBOL_DIR); `lob run`/`lob board` set it up.
"""

import argparse
import sys

from skidl import ERC, Net, Part, generate_netlist

RF_VALUE = "9k"  # feedback resistor (OUT → FB)
RG_VALUE = "1k"  # ground resistor (FB → GND)

R_FOOTPRINT = "Resistor_SMD:R_0805_2012Metric"
# TL072 in a SOIC-8 (one of the footprints its symbol declares).
U_FOOTPRINT = "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm"
U_MPN = "TL072CDR"  # TI TL072C, SOIC-8, tape & reel


def build():
    """Construct the non-inverting amplifier in the default SKiDL circuit."""
    opamp = Part(
        "Amplifier_Operational",
        "TL072",
        value="TL072",
        ref="U1",
        footprint=U_FOOTPRINT,
    )
    # The SPICE model travels with the part (resolved by core::symbols). Ideal
    # op-amp subckt for now, mapped onto the TL072's real unit-A pins.
    opamp.fields["Sim.Device"] = "SUBCKT"
    opamp.fields["Sim.Name"] = "kicad_builtin_opamp"
    opamp.fields["Sim.Library"] = "${KICAD9_SYMBOL_DIR}/Simulation_SPICE.sp"
    opamp.fields["Sim.Pins"] = "3=in+ 2=in- 8=vcc 4=vee 1=out"
    opamp.fields["MPN"] = U_MPN

    rf = Part("Device", "R", value=RF_VALUE, footprint=R_FOOTPRINT, ref="R1")
    rg = Part("Device", "R", value=RG_VALUE, footprint=R_FOOTPRINT, ref="R2")

    vin = Net("IN")
    vout = Net("OUT")
    fb = Net("FB")
    gnd = Net("GND")
    vcc = Net("VCC")
    vee = Net("VEE")

    # Unit A: the non-inverting amplifier.
    vin += opamp[3]  # non-inverting input (+)
    fb += opamp[2], rf[2], rg[1]  # inverting input (−), feedback node
    vout += opamp[1], rf[1]  # output, feedback resistor high side
    gnd += rg[2]  # ground resistor low side
    vcc += opamp[8]  # V+ rail (ignored by the ideal-VCVS sim model)
    vee += opamp[4]  # V− rail

    # Unit B is unused — terminate it as a grounded unity follower so its inputs
    # don't float (standard practice; keeps ERC and the real board clean).
    nb = Net("NB")
    gnd += opamp[5]  # +in(B) → GND
    nb += opamp[6], opamp[7]  # −in(B) tied to out(B)

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
