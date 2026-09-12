local function run()
  local repository = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  local workspace = repository .. '/nested-workspace'
  local main_path = workspace .. '/Main.pas'
  local config_path = repository .. '/.lint4d.toml'

  local capabilities = vim.lsp.protocol.make_client_capabilities()
  capabilities.workspace.workspaceFolders = true
  capabilities.workspace.didChangeWatchedFiles = {
    dynamicRegistration = true,
    relativePatternSupport = true,
  }

  vim.cmd.edit(vim.fn.fnameescape(main_path))
  vim.bo.filetype = 'pascal'
  local bufnr = vim.api.nvim_get_current_buf()
  local client_id = vim.lsp.start({
    name = 'pascal_lsp_external_watch_smoke',
    cmd = { assert(vim.env.PASCAL_LSP_BIN), '--stdio' },
    root_dir = workspace,
    workspace_folders = {
      { uri = vim.uri_from_fname(workspace), name = 'nested-workspace' },
    },
    capabilities = capabilities,
    filetypes = { 'pascal' },
  }, { bufnr = bufnr })
  assert(client_id, 'could not start Pascal LSP')

  assert(
    vim.wait(10000, function()
      return #vim.lsp.get_clients({ bufnr = bufnr, name = 'pascal_lsp_external_watch_smoke' }) == 1
    end),
    'Pascal LSP did not attach'
  )
  local client = vim.lsp.get_client_by_id(client_id)
  assert(client, 'Pascal LSP client disappeared')
  assert(
    vim.wait(10000, function()
      return client.dynamic_capabilities:get('workspace/didChangeWatchedFiles') ~= nil
    end),
    'Pascal LSP did not install the dynamic watcher'
  )

  local function has_constant_diagnostic()
    for _, diagnostic in ipairs(vim.diagnostic.get(bufnr)) do
      if diagnostic.code == 'constant-naming' then
        return true
      end
    end
    return false
  end

  assert(vim.wait(10000, has_constant_diagnostic), 'initial fallback diagnostics were not published')

  vim.fn.writefile({ '[rules]', 'constant-naming = "off"' }, config_path)
  assert(
    vim.wait(10000, function()
      return not has_constant_diagnostic()
    end),
    'creating the repository-parent configuration did not update diagnostics'
  )

  vim.fn.writefile({ '[rules.naming]', 'constant_style = "UPPER_CASE"' }, config_path)
  assert(
    vim.wait(10000, has_constant_diagnostic),
    'modifying the repository-parent configuration did not update diagnostics'
  )

  vim.fn.writefile({ '[rules]', 'constant-naming = "off"' }, config_path)
  assert(
    vim.wait(10000, function()
      return not has_constant_diagnostic()
    end),
    'clearing the repository-parent configuration did not update diagnostics before deletion'
  )

  assert(vim.fn.delete(config_path) == 0, 'could not delete repository-parent configuration')
  assert(
    vim.wait(10000, has_constant_diagnostic),
    'deleting the repository-parent configuration did not restore fallback diagnostics'
  )

  client:stop()
  assert(
    vim.wait(10000, function()
      return client:is_stopped()
    end),
    'Pascal LSP did not shut down'
  )
  io.stdout:write('NEOVIM_EXTERNAL_WATCH_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
