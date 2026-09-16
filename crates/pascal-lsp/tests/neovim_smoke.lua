local REQUEST_TIMEOUT = 10000
local BUSY_ERROR_CODE = -32803
local BUSY_RETRY_DELAY = 50
local MAX_BUSY_RETRIES = REQUEST_TIMEOUT / BUSY_RETRY_DELAY

local function wait_for_analysis_slot(client, uri, bufnr)
  for _ = 1, MAX_BUSY_RETRIES do
    local completed = false
    local callback_error
    local accepted = client:request('textDocument/documentSymbol', {
      textDocument = { uri = uri },
    }, function(err)
      callback_error = err
      completed = true
    end, bufnr)
    assert(accepted, 'analysis readiness request was not accepted')
    assert(vim.wait(REQUEST_TIMEOUT, function()
      return completed
    end), 'analysis readiness request timed out')
    if not callback_error then
      return
    end
    assert(callback_error.code == BUSY_ERROR_CODE,
      'analysis readiness request failed: ' .. vim.inspect(callback_error))
    vim.wait(BUSY_RETRY_DELAY)
  end
  error('analysis remained busy for ' .. REQUEST_TIMEOUT .. 'ms')
end

local function run()
  vim.cmd('filetype on')
  vim.opt.hidden = true
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))
  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  local provider_path = root .. '/Provider.pas'
  local consumer_path = root .. '/Main.pas'
  local provider_on_disk = vim.fn.readfile(provider_path)
  local consumer_on_disk = vim.fn.readfile(consumer_path)

  vim.cmd.edit(vim.fn.fnameescape(provider_path))
  local provider = vim.api.nvim_get_current_buf()
  assert(vim.wait(5000, function()
    return #vim.lsp.get_clients({ bufnr = provider, name = 'pascal_lsp' }) == 1
  end), 'LSP did not attach using the shipped example')
  local client = vim.lsp.get_clients({ bufnr = provider, name = 'pascal_lsp' })[1]
  assert(client.offset_encoding == 'utf-16', 'wrong position encoding')
  assert(vim.fn.bufnr(consumer_path) == -1, 'consumer must start unopened')
  wait_for_analysis_slot(client, vim.uri_from_fname(provider_path), provider)

  local renamed_provider = {
    'unit Provider;', 'interface', 'const', '  renamedConst = 1;',
    '  unrelatedConst = 2;', 'implementation', 'end.',
  }
  local renamed_consumer = {
    'unit Main;', 'interface', 'uses Provider;', 'implementation', 'procedure Run;',
    'begin', '  Log(renamedConst);', '  Log(unrelatedConst);', 'end;', 'end.',
  }
  vim.api.nvim_win_set_cursor(0, { 4, 3 })
  vim.lsp.buf.rename('renamedConst')
  assert(vim.wait(5000, function()
    local consumer = vim.fn.bufnr(consumer_path)
    return consumer > 0
      and vim.deep_equal(vim.api.nvim_buf_get_lines(provider, 0, -1, false), renamed_provider)
      and vim.deep_equal(vim.api.nvim_buf_get_lines(consumer, 0, -1, false), renamed_consumer)
  end), 'vim.lsp.buf.rename did not apply the complete workspace edit')

  local consumer = vim.fn.bufnr(consumer_path)
  assert(vim.bo[provider].modified, 'provider rename must remain unsaved')
  assert(vim.bo[consumer].modified, 'consumer rename must remain unsaved')
  assert(vim.deep_equal(vim.fn.readfile(provider_path), provider_on_disk), 'rename saved Provider.pas')
  assert(vim.deep_equal(vim.fn.readfile(consumer_path), consumer_on_disk), 'rename saved Main.pas')

  local fixed_provider = {
    'unit Provider;', 'interface', 'const', '  RENAMED_CONST = 1;',
    '  unrelatedConst = 2;', 'implementation', 'end.',
  }
  local fixed_consumer = {
    'unit Main;', 'interface', 'uses Provider;', 'implementation', 'procedure Run;',
    'begin', '  Log(RENAMED_CONST);', '  Log(unrelatedConst);', 'end;', 'end.',
  }
  vim.api.nvim_set_current_buf(provider)
  vim.api.nvim_win_set_cursor(0, { 4, 3 })
  wait_for_analysis_slot(client, vim.uri_from_fname(provider_path), provider)
  vim.lsp.buf.code_action({ apply = true, context = { only = { 'quickfix' } } })
  assert(vim.wait(5000, function()
    return vim.deep_equal(vim.api.nvim_buf_get_lines(provider, 0, -1, false), fixed_provider)
      and vim.deep_equal(vim.api.nvim_buf_get_lines(consumer, 0, -1, false), fixed_consumer)
  end), 'vim.lsp.buf.code_action did not apply its deterministic quick fix')

  assert(vim.bo[provider].modified, 'provider code action must remain unsaved')
  assert(vim.bo[consumer].modified, 'consumer code action must remain unsaved')
  assert(vim.deep_equal(vim.fn.readfile(provider_path), provider_on_disk), 'code action saved Provider.pas')
  assert(vim.deep_equal(vim.fn.readfile(consumer_path), consumer_on_disk), 'code action saved Main.pas')
  client:stop()
  assert(vim.wait(5000, function() return client:is_stopped() end), 'server did not shut down')
  io.stdout:write('NEOVIM_WORKSPACE_EDIT_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
