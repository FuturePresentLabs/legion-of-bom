-- Mouser Search API: live unit price + stock by MPN, or free-text keyword.
-- https://api.mouser.com/api/v1 -- docs: mouser.com/api-search/
--
-- Contract (crates/core/src/mouser.rs):
--   search_mpn(req)     -> part | nil        req = { api_key, mpn }
--   search_keyword(req) -> { part, ... }     req = { api_key, keyword, records }
--
-- `part` shape (matches PartPrice in mouser.rs exactly):
--   { mpn, manufacturer, in_stock, datasheet_url, product_url, image_url,
--     price_breaks = { { quantity, unit_price, currency }, ... } }
--
-- Also exported, pure and host-I/O-free, so the response-parsing logic is
-- directly testable against a JSON fixture (no live API key needed):
--   parse_search_response(req)  -> part | nil   req = { json, wanted }
--   parse_keyword_response(json) -> { part, ... }
--
-- Host primitives available here: http_request, json_encode, json_decode
-- (see crates/core/src/distributor_lua.rs) -- no other network or I/O access.

local SEARCH_URL = "https://api.mouser.com/api/v1/search/partnumber"
local KEYWORD_URL = "https://api.mouser.com/api/v1/search/keyword"

-- A non-empty `Errors` array in the response is Mouser's own error signal,
-- independent of HTTP status (Mouser answers 200 even for e.g. a bad key).
local function check_errors(json)
    if json and json.Errors and #json.Errors > 0 then
        local msgs = {}
        for _, e in ipairs(json.Errors) do
            if e.Message then
                msgs[#msgs + 1] = e.Message
            end
        end
        error(#msgs > 0 and table.concat(msgs, "; ") or "unknown error")
    end
end

-- "$1.48" / "$1,234.50" -> 1.48 / 1234.50. Assumes a `.`-decimal currency
-- (USD); strips the currency symbol and thousands separators.
local function parse_price(s)
    if not s then
        return nil
    end
    local cleaned = s:gsub("[^%d%.]", "")
    return tonumber(cleaned)
end

local function part_from_json(p)
    local price_breaks = {}
    if p.PriceBreaks then
        for _, b in ipairs(p.PriceBreaks) do
            local price = parse_price(b.Price)
            if b.Quantity and price then
                price_breaks[#price_breaks + 1] = {
                    quantity = b.Quantity,
                    unit_price = price,
                    currency = b.Currency or "USD",
                }
            end
        end
    end
    return {
        mpn = p.ManufacturerPartNumber or "",
        manufacturer = p.Manufacturer,
        in_stock = p.AvailabilityInStock and tonumber(p.AvailabilityInStock) or nil,
        datasheet_url = p.DataSheetUrl,
        product_url = p.ProductDetailUrl,
        image_url = p.ImagePath, -- the Visual BOM thumbnail source
        price_breaks = price_breaks,
    }
end

local function post_json(url, body_table)
    local resp = http_request({
        method = "POST",
        url = url,
        headers = { ["Content-Type"] = "application/json" },
        body = json_encode(body_table),
    })
    if resp.status ~= 200 then
        error("http " .. resp.status .. ": " .. resp.body)
    end
    return resp.body
end

-- Parses a raw `SearchByPartRequest` JSON response, preferring an exact MPN
-- match over Mouser's own top result. Pure -- no network -- so it's directly
-- testable against a fixture.
function parse_search_response(req)
    local json = json_decode(req.json)
    check_errors(json)

    local parts = json.SearchResults and json.SearchResults.Parts
    if not parts or #parts == 0 then
        return nil
    end
    for _, p in ipairs(parts) do
        if p.ManufacturerPartNumber == req.wanted then
            return part_from_json(p)
        end
    end
    return part_from_json(parts[1])
end

-- Parses a raw `SearchByKeywordRequest` JSON response into every candidate
-- part (the caller ranks and picks). Pure, same reason as above.
function parse_keyword_response(json_text)
    local json = json_decode(json_text)
    check_errors(json)

    local parts = json.SearchResults and json.SearchResults.Parts
    local out = {}
    if parts then
        for _, p in ipairs(parts) do
            out[#out + 1] = part_from_json(p)
        end
    end
    return out
end

-- Search by exact manufacturer part number; returns the best match (an exact
-- MPN hit if Mouser returned one, else its own top result).
function search_mpn(req)
    local url = SEARCH_URL .. "?apiKey=" .. req.api_key
    local body = post_json(url, {
        SearchByPartRequest = { mouserPartNumber = req.mpn, partSearchOptions = "" },
    })
    return parse_search_response({ json = body, wanted = req.mpn })
end

-- Free-text keyword search -- the path from a *generic* value ("10k resistor
-- 0805") to real MPNs, which search_mpn (exact-lookup) can't do. Returns up
-- to req.records in-stock candidates, ranked by Mouser's own relevance (the
-- caller re-ranks). records is clamped to Mouser's 1..=50 window; 0 means
-- "use the default (10)".
function search_keyword(req)
    local records = req.records
    if not records or records == 0 then
        records = 10
    elseif records > 50 then
        records = 50
    end

    local url = KEYWORD_URL .. "?apiKey=" .. req.api_key
    local body = post_json(url, {
        SearchByKeywordRequest = {
            keyword = req.keyword,
            records = records,
            startingRecord = 0,
            searchOptions = "InStock",
        },
    })
    return parse_keyword_response(body)
end
