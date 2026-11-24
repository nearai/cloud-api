#!lua name=spendtotal
-- Running spend totals shared by the processes of one deployment.
-- Keys of one API key: total, total..':pend' (costs recorded while a seed
-- runs) and total..':gen' (a token that changes whenever the total is written
-- or dropped). Every key written here has an expiry; a total or pend key found
-- without one was not written by this library and is dropped.
-- Amounts are decimal integers handled by the server (INCRBY), never Lua
-- numbers, so values above 2^53 stay exact.

local function is_int(s)
  return type(s) == 'string' and string.match(s, '^%-?%d+$') ~= nil
end

local function live(key)
  local v = redis.call('GET', key)
  if v and redis.call('TTL', key) < 0 then
    redis.call('DEL', key)
    return false
  end
  return v
end

-- KEYS: total. Returns the total, or nil.
local function get(keys)
  return live(keys[1])
end

-- KEYS: pend, gen. ARGV: pend ttl, gen ttl, new token.
-- Starts (or continues) collecting costs and returns the current token.
local function begin(keys, args)
  local g = redis.call('GET', keys[2]) or args[3]
  redis.call('SET', keys[2], g, 'EX', args[2])
  local p = live(keys[1]) or '0'
  redis.call('SET', keys[1], p, 'EX', args[1])
  return g
end

-- KEYS: total, pend. ARGV: cost.
-- Adds to each key that exists; never creates one. Returns 1 for total, +2 for pend.
local function add(keys, args)
  if not is_int(args[1]) then return redis.error_reply('ERR spend: bad cost') end
  local n = 0
  if live(keys[1]) then redis.call('INCRBY', keys[1], args[1]); n = n + 1 end
  if live(keys[2]) then redis.call('INCRBY', keys[2], args[1]); n = n + 2 end
  return n
end

-- KEYS: total, pend, gen. ARGV: sum, total ttl, expected token, gen ttl, new token.
-- Writes sum + pend unless the token changed, pend is gone or a total exists.
-- Returns the new total, or nil when it does nothing.
local function finish(keys, args)
  if not is_int(args[1]) then return redis.error_reply('ERR spend: bad sum') end
  if redis.call('GET', keys[3]) ~= args[3] then return false end
  local p = live(keys[2])
  if not p then return false end
  if not is_int(p) then return redis.error_reply('ERR spend: bad pend') end
  if live(keys[1]) then return false end
  redis.call('SET', keys[1], args[1], 'EX', args[2])
  redis.call('INCRBY', keys[1], p)
  redis.call('DEL', keys[2])
  redis.call('SET', keys[3], args[5], 'EX', args[4])
  return redis.call('GET', keys[1])
end

-- KEYS: total, pend, gen. ARGV: gen ttl, new token.
-- Drops the total and pend and changes the token, so a seed in flight aborts.
local function invalidate(keys, args)
  redis.call('DEL', keys[1])
  redis.call('DEL', keys[2])
  redis.call('SET', keys[3], args[2], 'EX', args[1])
  return 1
end

redis.register_function('spend_v2_get', get)
redis.register_function('spend_v2_begin', begin)
redis.register_function('spend_v2_add', add)
redis.register_function('spend_v2_finish', finish)
redis.register_function('spend_v2_invalidate', invalidate)
