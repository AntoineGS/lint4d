local function run()
  vim.cmd('filetype on')
  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  local provider = assert(vim.env.PASCAL_LSP_EXPECTED_PROVIDER)
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))
  vim.cmd.edit(vim.fn.fnameescape(root .. '/Main.pas'))
  assert(vim.wait(10000, function()
    local clients = vim.lsp.get_clients({ bufnr = 0, name = 'pascal_lsp' })
    return clients[1] ~= nil and clients[1].initialized
  end, 20), 'pascal_lsp did not attach')
  local line = vim.api.nvim_buf_get_lines(0, 2, 3, false)[1]
  local column = assert(line:find('Provider', 1, true)) - 1
  vim.api.nvim_win_set_cursor(0, { 3, column })
  vim.lsp.buf.definition()
  assert(vim.wait(10000, function()
    return vim.api.nvim_buf_get_name(0) == provider
  end, 20), 'definition did not open mapped native provider')
  print('NEOVIM_DELPHI_OVERRIDES_OK')
  io.stdout:write('NEOVIM_DELPHI_OVERRIDES_OK\n')
  vim.cmd('qa!')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
end
