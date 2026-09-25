-- Bounded aerospace environment/assurance policy. Geometry, units, numeric
-- validation and calculations remain Rust-owned.
--
-- @derives-from url:https://ecss.nl/standard/ecss-e-st-10-04c-rev-1-space-environment-15-june-2020/ Scope -- mission-specific natural/induced environment must be supplied; this profile does not invent loads
-- @derives-from url:https://ecss.nl/standard/ecss-q-st-60-15c-rev-1-radiation-hardness-assurance-20-march-2025/ Scope -- RHA is project-specific and covers TID, TNID and SEE for space projects
-- @see url:https://ecss.nl/standard/ecss-q-st-30-11c-rev-2-derating-eee-components-23-june-2021/
-- @see url:https://ecss.nl/standard/ecss-q-st-70-12c-rev-1-design-rules-for-printed-circuit-boards-30-april-2025/
-- @derives-from url:https://standards.nasa.gov/sites/default/files/standards/NASA/Baseline/0/nasa-std-873910.pdf §§1.1, 5-8 -- part-selection, source, traceability and risk evidence obligations only
-- @see url:https://standards.nasa.gov/standard/NASA/NASA-STD-87391 -- polymeric applications, staking and coating process evidence
-- @see url:https://standards.nasa.gov/node/283 -- crimp, cable and harness workmanship evidence
-- @see url:https://standards.nasa.gov/standard/NASA/NASA-STD-87396 -- process/personnel qualification and implementation remain external
-- @derives-from url:https://standards.nasa.gov/NASA-inactive-cancelled-standards -- NASA-STD-8739.2/.3 are inactive and never selected as defaults

local function check(id, designation, applicability, reason, locator)
    return {
        id = id,
        designation = designation,
        applicability = applicability,
        reason = reason,
        source_locator = locator,
    }
end

local function evidence(id, kind, scope, applicability, reason, locator, required_fields, required_risks)
    return {
        id = id,
        kind = kind,
        scope = scope,
        required_fields = required_fields or {},
        required_risks = required_risks or {},
        applicability = applicability,
        reason = reason,
        source_locator = locator,
    }
end

function profile(request)
    local mission = request.selections.mission_class
    local radiation = nil
    local vacuum = nil
    if mission == "educational_suborbital" then
        radiation = "not_applicable"
        vacuum = "conditional"
    elseif mission == "sounding_rocket" then
        radiation = "conditional"
        vacuum = "conditional"
    elseif mission == "orbital_experimental" or mission == "orbital_high_reliability" or mission == "human_rated" then
        radiation = "required"
        vacuum = "required"
    else
        return nil
    end
    local legacy_2 = request.flags.legacy_nasa_8739_2 and "conditional" or "not_applicable"
    local legacy_3 = request.flags.legacy_nasa_8739_3 and "conditional" or "not_applicable"

    return {
        id = "aerospace",
        facts = {
            { id = "environment.temperature_range", reason = "component stress and derating require bounded mission temperatures" },
            { id = "environment.pressure_range", reason = "vacuum and high-altitude applicability derives from mission pressure" },
            { id = "environment.vibration_spectrum", reason = "mechanical verification uses the actual mission spectrum, not a profile guess" },
            { id = "environment.shock_spectrum", reason = "mounting and retention checks require the actual mission shock spectrum" },
            { id = "electrical.bus_envelope", reason = "derating and transient checks use worst-case mission bus limits" },
        },
        checks = {
            check("eee_derating", "ECSS-Q-ST-30-11C Rev.2", "required", "calculate EEE stress ratios from mission limits", "ECSS-Q-ST-30-11C Rev.2 scope and applicable component clauses"),
            check("aerospace_pcb_geometry", "ECSS-Q-ST-70-12C Rev.1", "required", "apply the bounded artifact-checkable PCB profile selected for the construction", "ECSS-Q-ST-70-12C Rev.1 scope; construction-specific clauses"),
            check("radiation_hardness_assurance", "ECSS-Q-ST-60-15C Rev.1", radiation, "orbital missions require project-specific TID, TNID and SEE assurance; non-orbital missions require explicit applicability", "ECSS-Q-ST-60-15C Rev.1 scope"),
            check("legacy_nasa_8739_2", "NASA-STD-8739.2 (inactive)", legacy_2, "inactive workmanship documents are never defaults; explicit governing authority and approval are required for legacy use", "NASA inactive/cancelled standards register"),
            check("legacy_nasa_8739_3", "NASA-STD-8739.3 (inactive)", legacy_3, "inactive workmanship documents are never defaults; explicit governing authority and approval are required for legacy use", "NASA inactive/cancelled standards register"),
        },
        evidence = {
            evidence("eee_part_traceability", "supplier", "part", "required", "each installed EEE part needs source, grade, lot/date and generic risk dispositions for pure tin and PEM use", "NASA-STD-8739.10 selection and traceability scope", { "manufacturer", "grade", "authorized_source", "traceability_record", "lot_code", "date_code" }, { "pure_tin", "pem" }),
            evidence("substitution_approval", "review", "package", "conditional", "actual installed substitutions need a traceable approval record", "NASA-STD-8739.10 parts-control scope"),
            evidence("assembly_inspection_and_test", "test", "package", "required", "as-built workmanship needs inspection and functional-test records", "NASA-STD-8739.6B implementation/assurance scope"),
            evidence("process_and_personnel_qualification", "supplier", "package", "required", "Lob cannot establish process or operator qualification", "NASA-STD-8739.6B implementation/assurance scope"),
            evidence("polymerics_staking_and_coating", "supplier", "package", "conditional", "used polymeric applications, staking and conformal coating need controlled process records", "NASA-STD-8739.1B scope"),
            evidence("crimp_and_harness_workmanship", "supplier", "package", "conditional", "used crimped interconnects and harnesses need controlled workmanship records", "NASA-STD-8739.4A scope"),
            evidence("legacy_nasa_8739_2_approval", "review", "package", legacy_2, "legacy use requires governing requirement and approval", "NASA inactive/cancelled standards register"),
            evidence("legacy_nasa_8739_3_approval", "review", "package", legacy_3, "legacy use requires governing requirement and approval", "NASA inactive/cancelled standards register"),
            evidence("mission_environment_specification", "review", "package", "required", "loads must be project-provided and traceable", "ECSS-E-ST-10-04C Rev.1 scope"),
            evidence("vibration_and_shock_test", "test", "package", "required", "CAD checks do not demonstrate assembled hardware survival", "mission-specific verification plan"),
            evidence("thermal_vacuum_test", "test", "package", vacuum, "pressure and temperature exposure require physical evidence when applicable", "mission-specific verification plan"),
            evidence("radiation_analysis_and_test", "test", "package", radiation, "RHA requires project-specific analysis and component evidence", "ECSS-Q-ST-60-15C Rev.1 scope and Annex B DRD"),
            evidence("pcb_supplier_process_and_coupon", "supplier", "package", "required", "PCB CAD geometry does not prove manufacturing process capability", "ECSS-Q-ST-70-12C Rev.1 clauses 15.1-15.2"),
        },
        claim = "Selected aerospace design and external-evidence obligations only; unresolved evidence remains visible.",
    }
end
