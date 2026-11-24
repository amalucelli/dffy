local util = require("util")

LIMIT = 10
local counter = 0

function top(a)
  local function inner() end
  return a
end

local function helper() end

local M = {}

M.version = "1.0"

function M.area(p)
  return 0
end

function M:draw()
end

M.scale = function(p) end

