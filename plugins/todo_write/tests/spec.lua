local TodoPrompt = require("todo_prompt")

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    table.insert(failures, name .. ": " .. tostring(err))
  end
end

local function eq(actual, expected, msg)
  if actual ~= expected then
    error((msg or "") .. "\nexpected: " .. tostring(expected) .. "\n  actual: " .. tostring(actual))
  end
end

local function todo(status, content)
  return { status = status, content = content }
end

local CAP = TodoPrompt.MAX_TOTAL_BYTES

case("small_list_passes_through", function()
  local todos = {
    todo("in_progress", "do a thing"),
    todo("pending", "do another"),
    todo("completed", "done already"),
  }
  local out = TodoPrompt.build(todos)
  assert(type(out) == "string", "expected rendered block")
  assert(out:find("do a thing", 1, true), "in_progress todo must be present")
  assert(out:find("do another", 1, true), "pending todo must be present")
  assert(#out <= CAP, "block must be within cap")
end)

case("empty_and_invalid_todos_render_nil", function()
  eq(TodoPrompt.build({}), nil)
  eq(TodoPrompt.build({ { status = "bogus", content = "x" }, { content = "no status" } }), nil)
end)

case("pending_dropped_before_in_progress", function()
  local big = string.rep("p", CAP)
  local out = TodoPrompt.build({
    todo("in_progress", "keep me"),
    todo("pending", big),
  })
  assert(type(out) == "string")
  assert(out:find("keep me", 1, true), "in_progress must survive")
  assert(not out:find(big, 1, true), "oversized pending must be dropped")
  assert(#out <= CAP)
end)

case("two_oversized_in_progress_stay_within_cap", function()
  local big = string.rep("x", CAP)
  local out = TodoPrompt.build({
    todo("in_progress", big .. "first"),
    todo("in_progress", big .. "second"),
  })
  assert(type(out) == "string", "expected a rendered block")
  assert(#out <= CAP, "block must be within cap, got " .. #out)
  assert(out:find("in_progress", 1, true), "at least one in_progress entry must remain")
end)

case("all_in_progress_oversized_stays_within_cap", function()
  local big = string.rep("y", CAP)
  local out = TodoPrompt.build({
    todo("in_progress", "first-" .. big),
    todo("in_progress", "second-" .. big),
    todo("in_progress", "third-" .. big),
  })
  assert(type(out) == "string")
  assert(#out <= CAP, "block must be within cap, got " .. #out)
end)

case("oversized_content_is_ellipsis_truncated_not_dropped", function()
  local marker = "ZebraMarker"
  local big = marker .. string.rep("z", CAP)
  local out = TodoPrompt.build({ todo("in_progress", big) })
  assert(type(out) == "string")
  assert(#out <= CAP)
  assert(out:find(marker, 1, true), "truncated entry must keep leading content")
  assert(out:find("...", 1, true), "truncated entry must carry ellipsis")
end)

case("multibyte_content_shrinks_to_valid_json", function()
  local big = string.rep("é", CAP)
  local out = TodoPrompt.build({ todo("in_progress", big) })
  assert(type(out) == "string")
  assert(#out <= CAP, "block must be within cap, got " .. #out)
  local line = out:match('\n({"status"[^\n]*)')
  assert(line, "a shrunk todo line must remain")
  local decoded = n00n.json.decode(line)
  assert(type(decoded) == "table" and decoded.status == "in_progress", "shrunk line must stay decodable JSON")
end)

case("completed_dropped_when_over_cap_but_pending_first", function()
  local filler = string.rep("c", CAP)
  local out = TodoPrompt.build({
    todo("completed", filler),
    todo("pending", "p1 " .. filler),
    todo("in_progress", "active"),
  })
  assert(type(out) == "string")
  assert(#out <= CAP)
  assert(out:find("active", 1, true), "in_progress must survive")
  assert(not out:find("p1", 1, true), "pending drops before completed")
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
