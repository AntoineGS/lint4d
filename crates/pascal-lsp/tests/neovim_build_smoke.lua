local REQUEST_TIMEOUT = 10000
local BUSY_ERROR_CODE = -32803
local BUSY_RETRY_DELAY = 50

local function run()
  vim.cmd('filetype on')
  local pickers = {}
  local notifications = {}
  local original_select = vim.ui.select
  local original_notify = vim.notify
  vim.ui.select = function(items, options, callback)
    options = options or {}
    pickers[#pickers + 1] = { items = items, options = options }
    if type(options.prompt) == 'string' and options.prompt:match('^Build configuration %(') then
      local debug_choice
      for _, item in ipairs(items) do
        if item.value == 'Debug' then
          debug_choice = item
          break
        end
      end
      assert(debug_choice, 'build configuration picker did not offer Debug: ' .. vim.inspect(items))
      callback(debug_choice)
    elseif type(options.prompt) == 'string' and options.prompt:match('^Platform %(') then
      local win32_choice
      for _, item in ipairs(items) do
        if item.value == 'Win32' then
          win32_choice = item
          break
        end
      end
      assert(win32_choice, 'platform picker did not offer Win32: ' .. vim.inspect(items))
      callback(win32_choice)
    elseif type(options.prompt) == 'string' and options.prompt:match('^Pascal project %(') then
      callback(nil)
    else
      callback(nil)
    end
  end
  vim.notify = function(message, level)
    notifications[#notifications + 1] = { message = tostring(message), level = level }
  end
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))

  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  local project_path = root .. '/App.dproj'
  local source_path = root .. '/App.dpr'
  local project_uri = vim.uri_from_fname(project_path)

  local function request(client, method, params, bufnr)
    local result
    local response_error
    local completed = false
    local function attempt()
      local accepted = client:request(method, params, function(err, response)
        if err and err.code == BUSY_ERROR_CODE then
          vim.defer_fn(attempt, BUSY_RETRY_DELAY)
          return
        end
        response_error = err
        result = response
        completed = true
      end, bufnr)
      assert(accepted, method .. ' request was not accepted')
    end
    attempt()
    assert(vim.wait(REQUEST_TIMEOUT, function()
      return completed
    end), method .. ' request timed out')
    assert(not response_error, method .. ' request failed: ' .. vim.inspect(response_error))
    return result
  end

  vim.cmd.edit(vim.fn.fnameescape(source_path))
  local bufnr = vim.api.nvim_get_current_buf()
  assert(vim.wait(REQUEST_TIMEOUT, function()
    return #vim.lsp.get_clients({ bufnr = bufnr, name = 'pascal_lsp' }) == 1
  end), 'LSP did not attach using the shipped example')
  local client = vim.lsp.get_clients({ bufnr = bufnr, name = 'pascal_lsp' })[1]
  assert(client.server_capabilities.experimental.buildSelection == true, 'server did not advertise buildSelection')

  local context = request(client, 'pascal/buildContext', { projectUri = project_uri }, bufnr)
  assert(context.config.selected == 'Release', vim.inspect(context.config))
  assert(vim.deep_equal(context.config.candidates, { 'Debug', 'Release' }), vim.inspect(context.config.candidates))

  local commands = vim.api.nvim_buf_get_commands(bufnr, {})
  assert(commands.PascalBuildConfig, ':PascalBuildConfig was not created')
  assert(commands.PascalPlatform, ':PascalPlatform was not created')
  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd.PascalBuildConfig()
  end)

  assert(vim.wait(REQUEST_TIMEOUT, function()
    for _, notification in ipairs(notifications) do
      if notification.message:find('Build configuration: Debug (session)', 1, true) then
        return true
      end
    end
    return false
  end), 'Debug session selection was not reported: ' .. vim.inspect(notifications))

  context = request(client, 'pascal/buildContext', { projectUri = project_uri }, bufnr)
  assert(context.config.selected == 'Debug', vim.inspect(context.config))
  assert(context.config.mode == 'session', vim.inspect(context.config))

  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd.PascalPlatform()
  end)
  assert(vim.wait(REQUEST_TIMEOUT, function()
    for _, notification in ipairs(notifications) do
      if notification.message:find('Platform: Win32 (session)', 1, true) then
        return true
      end
    end
    return false
  end), 'Win32 session selection was not reported: ' .. vim.inspect(notifications))

  context = request(client, 'pascal/buildContext', { projectUri = project_uri }, bufnr)
  assert(context.platform.selected == 'Win32', vim.inspect(context.platform))
  assert(context.platform.mode == 'session', vim.inspect(context.platform))
  assert(context.config.selected == 'Debug', vim.inspect(context.config))
  assert(context.config.mode == 'session', vim.inspect(context.config))

  vim.api.nvim_buf_call(bufnr, function()
    vim.cmd.PascalProject()
  end)
  local project_picker
  assert(vim.wait(REQUEST_TIMEOUT, function()
    for _, picker in ipairs(pickers) do
      if picker.options.prompt and picker.options.prompt:match('^Pascal project %(') then
        project_picker = picker
        return true
      end
    end
    return false
  end), ':PascalProject did not open')
  local menu_labels = vim.tbl_map(function(item)
    return item.label
  end, project_picker.items)
  assert(vim.tbl_contains(menu_labels, 'Select build configuration'), 'project menu omitted build configuration picker')
  assert(vim.tbl_contains(menu_labels, 'Select platform'), 'project menu omitted platform picker')

  vim.ui.select = original_select
  vim.notify = original_notify
  client:stop()
  assert(vim.wait(5000, function()
    return client:is_stopped()
  end), 'server did not shut down')
  io.stdout:write('NEOVIM_BUILD_SELECTION_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
