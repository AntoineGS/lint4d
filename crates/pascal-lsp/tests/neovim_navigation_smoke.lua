local function run()
  vim.cmd('filetype on')
  vim.opt.hidden = true
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))
  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  vim.cmd.edit(vim.fn.fnameescape(root .. '/Main.pas'))
  local main = vim.api.nvim_get_current_buf()
  local provider_path = root .. '/Provider.pas'
  assert(vim.wait(5000, function()
    return #vim.lsp.get_clients({ bufnr = main, name = 'pascal_lsp' }) == 1
  end), 'LSP did not attach using the shipped example')
  local client = vim.lsp.get_clients({ bufnr = main, name = 'pascal_lsp' })[1]
  assert(client.offset_encoding == 'utf-16', 'wrong position encoding')

  local function at_provider_location(expected_line)
    return vim.api.nvim_buf_get_name(0) == provider_path
      and vim.api.nvim_win_get_cursor(0)[1] == expected_line
  end

  local function quickfix_item_path(item)
    if item.filename and item.filename ~= '' then
      return item.filename
    end
    if item.bufnr and item.bufnr > 0 and vim.api.nvim_buf_is_valid(item.bufnr) then
      return vim.api.nvim_buf_get_name(item.bufnr)
    end
    return nil
  end

  local function has_current_implementation_result(quickfix, expected_line)
    local context = quickfix.context
    if type(context) == 'table'
      and context.method
      and context.method ~= 'textDocument/implementation'
    then
      return false
    end
    local items = quickfix.items or {}
    return #items == 1
      and quickfix_item_path(items[1]) == provider_path
      and items[1].lnum == expected_line
  end

  local function keys(mapping, expected_line)
    local implementation_quickfix = mapping == 'gi'
    if implementation_quickfix then
      -- Neovim 0.13 lists implementations instead of jumping to a single one.
      -- Clear the previous gd/gD result so a stale quickfix entry cannot satisfy
      -- this smoke before the actual gi response arrives.
      vim.fn.setqflist({}, 'r')
    end
    vim.api.nvim_feedkeys(mapping, 'xt', false)
    local selected_implementation = false
    assert(vim.wait(5000, function()
      if at_provider_location(expected_line) then
        return true
      end
      if implementation_quickfix and not selected_implementation then
        local quickfix = vim.fn.getqflist({ items = 1, context = 1 })
        if has_current_implementation_result(quickfix, expected_line) then
          vim.cmd('cfirst')
          selected_implementation = true
        end
      end
      return at_provider_location(expected_line)
    end), mapping .. ' did not navigate to Provider.pas:' .. expected_line)
  end

  vim.api.nvim_win_set_cursor(0, { 7, 3 })
  keys('gd', 5)
  keys('gD', 3)
  keys('gi', 5)

  local provider = vim.api.nvim_get_current_buf()
  local lines = vim.api.nvim_buf_get_lines(provider, 0, -1, false)
  for i, line in ipairs(lines) do
    lines[i] = line:gsub('Greet', 'Changed')
  end
  vim.api.nvim_buf_set_lines(provider, 0, -1, false, lines)
  -- A request flushes this buffer's debounced didChange before switching buffers.
  local declaration = client:request_sync('textDocument/declaration', {
    textDocument = { uri = vim.uri_from_bufnr(provider) },
    position = { line = 4, character = 11 },
  }, 5000, provider)
  assert(declaration and not declaration.err and #declaration.result == 1, 'changed declaration unresolved')
  vim.api.nvim_set_current_buf(main)
  vim.api.nvim_buf_set_lines(main, 6, 7, false, { '  Changed;' })
  vim.api.nvim_win_set_cursor(0, { 7, 3 })
  keys('gd', 5)
  assert(vim.api.nvim_buf_get_lines(provider, 4, 5, false)[1] == 'procedure Changed;')
  assert(vim.fn.readfile(root .. '/Provider.pas')[5] == 'procedure Greet;', 'server wrote unsaved text')

  vim.api.nvim_set_current_buf(main)
  vim.api.nvim_buf_set_lines(main, 6, 7, false, { '  with Missing do Changed;' })
  assert(vim.wait(5000, function()
    for _, diagnostic in ipairs(vim.diagnostic.get(main)) do
      if diagnostic.code == 'with-statement' then
        return diagnostic.lnum == 6 and diagnostic.col == 2
      end
    end
    return false
  end), 'lint diagnostics did not reach Neovim')
  local response = client:request_sync('textDocument/formatting', {
    textDocument = { uri = vim.uri_from_bufnr(main) },
    options = { tabSize = 2, insertSpaces = true },
  }, 5000, main)
  assert(response and not response.err and response.result, 'formatting request failed')
  client:stop()
  assert(vim.wait(5000, function() return client:is_stopped() end), 'server did not shut down')
  io.stdout:write('NEOVIM_LSP_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
