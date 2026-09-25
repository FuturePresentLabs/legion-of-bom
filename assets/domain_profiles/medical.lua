-- Bounded medical-electronics applicability policy. It selects obligations;
-- it does not contain leakage-current math, EMC levels, or risk decisions.
--
-- @derives-from url:https://webstore.iec.ch/en/iec_catalog/product/preview/?id=L3B1Yi9wZGYvcHJldmlldy9pbmZvX2llYzYwNjAxLTF7ZWQzLjJ9ZW4ucGRm IEC 60601-1 Ed.3.2 §§1.1-1.3 and interpretation of 4.3 -- basic safety/essential performance scope and need to identify safety-relevant clinical performance
-- @derives-from url:https://webstore.iec.ch/en/publication/67554 IEC 60601-1-2 Ed.4.1 scope -- EMC emissions/immunity tests and intended-use environment categories
-- @derives-from url:https://www.iso.org/standard/72704.html ISO 14971:2019 scope -- lifecycle hazard identification, risk evaluation/control and effectiveness monitoring; no acceptable risk level invented here

local function check(id, designation, reason, locator)
    return {
        id = id,
        designation = designation,
        applicability = "required",
        reason = reason,
        source_locator = locator,
    }
end

local function evidence(id, kind, reason, locator)
    return {
        id = id,
        kind = kind,
        scope = "package",
        required_fields = {},
        required_risks = {},
        applicability = "required",
        reason = reason,
        source_locator = locator,
    }
end

function profile(request)
    local intended = request.selections.intended_use
    local contact = request.selections.contact
    local applied = request.selections.applied_part
    local environment = request.selections.use_environment
    local essential = request.flags.essential_performance

    if intended ~= "diagnosis" and intended ~= "monitoring" and intended ~= "treatment" and intended ~= "support" then return nil end
    if contact ~= "none" and contact ~= "operator" and contact ~= "patient" then return nil end
    if applied ~= "none" and applied ~= "b" and applied ~= "bf" and applied ~= "cf" then return nil end
    if environment ~= "professional" and environment ~= "home" and environment ~= "ems" and environment ~= "special" then return nil end
    if essential == nil then return nil end
    if contact == "patient" and applied == "none" then return nil end
    if contact ~= "patient" and applied ~= "none" then return nil end

    local facts = {
        { id = "medical.intended_use", reason = "basic-safety, essential-performance and risk analysis are evaluated against intended use" },
        { id = "medical.use_environment", reason = "EMC immunity selection depends on the intended-use environment" },
        { id = "environment.temperature_range", reason = "generic thermal calculations require the declared operating environment" },
        { id = "electrical.supply_envelope", reason = "generic electrical stress calculations require declared supply limits" },
    }
    local checks = {
        check("medical_basic_safety", "IEC 60601-1:2005+A1:2012+A2:2020 (Ed.3.2)", "screen design inputs affecting basic safety", "IEC 60601-1 Ed.3.2 clause 1.1"),
        check("medical_emc_risk", "IEC 60601-1-2:2014+A1:2020 (Ed.4.1)", "identify EMC design controls and environment-specific verification", "IEC 60601-1-2 Ed.4.1 scope"),
        check("medical_risk_management", "ISO 14971:2019", "identify hazards, evaluate/control risk and monitor control effectiveness over the lifecycle", "ISO 14971:2019 scope"),
    }
    local evidence_items = {
        evidence("risk_management_file", "review", "risk acceptability and control effectiveness require manufacturer review evidence", "ISO 14971:2019 scope"),
        evidence("basic_safety_test_report", "test", "artifact checks do not establish electrical/mechanical basic safety", "IEC 60601-1 Ed.3.2 clause 1.1"),
        evidence("emc_immunity_and_emissions_test", "test", "EMC performance must be tested for the declared intended-use environment", "IEC 60601-1-2 Ed.4.1 scope"),
        evidence("applicable_particular_standard_review", "review", "IEC 60601-1 states that applicable particular standards supplement or modify the general standard", "IEC 60601-1 Ed.3.2 clauses 1.2-1.3"),
    }

    if contact == "patient" then
        facts[#facts + 1] = { id = "medical.applied_part_class", reason = "patient-contact safeguards and tests require the selected B/BF/CF classification" }
        checks[#checks + 1] = check("applied_part_class", "IEC 60601-1:2005+A1:2012+A2:2020 (Ed.3.2)", "check the design against the selected applied-part classification", "IEC 60601-1 Ed.3.2 definitions and applied-part requirements")
        evidence_items[#evidence_items + 1] = evidence("patient_leakage_and_dielectric_test", "test", "patient-contact limits require physical measurement for the selected class", "IEC 60601-1 Ed.3.2 applied-part test requirements")
    end
    if essential then
        facts[#facts + 1] = { id = "medical.essential_performance_limits", reason = "the manufacturer must define safety-relevant clinical performance and its limits" }
        checks[#checks + 1] = check("essential_performance", "IEC 60601-1:2005+A1:2012+A2:2020 (Ed.3.2)", "identify safety-relevant clinical performance and explicit pass/fail limits", "IEC 60601-1 Ed.3.2 interpretation of clause 4.3")
        evidence_items[#evidence_items + 1] = evidence("essential_performance_verification", "test", "defined limits must be verified under applicable normal, fault and EMC conditions", "IEC 60601-1 Ed.3.2 interpretation of clause 4.3; IEC 60601-1-2 Ed.4.1 scope")
    end

    return {
        id = "medical",
        facts = facts,
        checks = checks,
        evidence = evidence_items,
        claim = "Selected medical design, risk-review and laboratory-evidence obligations only; unresolved evidence remains visible.",
    }
end
