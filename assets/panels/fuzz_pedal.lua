-- Classic guitar-pedal front panel: audio jacks on the left/right edges, DC
-- power at the top edge, 2 pots in the middle, LED + footswitch at the
-- bottom. Real classic-stompbox convention, not an arbitrary arrangement --
-- cross-checked this session against a real vendor drilling template
-- (Aion Electronics' Solaris Fuzz Face clone: IN on the right side, OUT on
-- the left, matches this script exactly).
--
-- `spec` (passed in by the host, see crates/core/src/panel_lua.rs):
--   width_mm, height_mm, edge_mm, gap_mm  -- real enclosure geometry
--   pot_refdes = {a, b}                   -- board parts to anchor at the pots
--   hardware.{jack,dc_jack,pot,footswitch,led}.{diameter_mm, envelope_mm}
--     -- real, verified hole/envelope sizes -- this script positions
--     -- controls, it never invents a hole size.
--
-- Editing this file changes the layout the next time `lob panel derive` /
-- `lob board` runs -- no Rust recompile.

function layout(spec)
    local hw = spec.hardware
    local edge = spec.edge_mm
    local gap = spec.gap_mm
    local w, h = spec.width_mm, spec.height_mm

    local pot_y = h * 0.55
    local footswitch_y = edge + hw.footswitch.envelope_mm[2] / 2 + 4.0
    local led_y = footswitch_y + hw.footswitch.envelope_mm[2] / 2 + gap + hw.led.envelope_mm[2] / 2
    -- Side jacks sit between the LED/footswitch cluster and the pot row --
    -- clear of both, not sharing either one's height.
    local jack_y = led_y + (pot_y - led_y) * 0.5
    local jack_x_left = edge + hw.jack.envelope_mm[1] / 2
    local jack_x_right = w - edge - hw.jack.envelope_mm[1] / 2
    local power_y = h - edge - hw.dc_jack.envelope_mm[2] / 2

    local pot_x = centered_row(w, 2, hw.pot.envelope_mm[1])

    return {
        { x_mm = jack_x_right, y_mm = jack_y, footprint = "Jack_6.35mm_TS", label = "IN", role = "io" },
        { x_mm = jack_x_left, y_mm = jack_y, footprint = "Jack_6.35mm_TS", label = "OUT", role = "io" },
        { x_mm = w / 2, y_mm = power_y, footprint = "DC_Jack_5.5x2.1mm", label = "9V" },
        { x_mm = pot_x[1], y_mm = pot_y, footprint = "Potentiometer_16mm", refdes = spec.pot_refdes[1], label = "FUZZ", role = "knob" },
        { x_mm = pot_x[2], y_mm = pot_y, footprint = "Potentiometer_16mm", refdes = spec.pot_refdes[2], label = "VOLUME", role = "knob" },
        { x_mm = w / 2, y_mm = led_y, footprint = "LED_5mm" },
        { x_mm = w / 2, y_mm = footswitch_y, footprint = "Footswitch_3PDT", role = "switch" },
    }
end
