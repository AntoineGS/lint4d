-- Neovim 0.11+. Install pascal-lsp on PATH, or set PASCAL_LSP_BIN to its full path.
-- Load this file from init.lua using dofile('/path/to/examples/neovim.lua').
-- For tool-neutral Delphi path/property overrides, see ../README.md's
-- "Delphi path overrides (LSP only)" section and configure delphi-tools files;
-- do not add mapping fields to init_options.
vim.filetype.add({ extension = { pas = 'pascal', dpr = 'pascal', dpk = 'pascal' } })
local example_file = assert(debug.getinfo(1, 'S').source:match('^@(.+)$'))
local pascal_project = dofile(vim.fn.fnamemodify(example_file, ':h') .. '/pascal_project.lua')

vim.lsp.config('pascal_lsp', {
  cmd = { vim.env.PASCAL_LSP_BIN or 'pascal-lsp', '--stdio' },
  filetypes = { 'pascal' },
  root_markers = { '.lint4d.toml', '.git' },
  init_options = {
    -- Optional Delphi project selection. Relative paths are relative to the
    -- workspace; omit these to discover the nearest unambiguous project.
    -- projectFile = 'src/Shop.dproj',
    -- buildConfig = 'Debug',
    -- platform = 'Win32',
    -- Additional source directories; relative paths are relative to the workspace.
    -- Add Delphi RTL/VCL source and third-party library source here when available.
    sourcePaths = {},
    exclude = {},
  },
  on_attach = function(client, buffer)
    vim.keymap.set('n', 'gd', vim.lsp.buf.definition, { buffer = buffer, desc = 'Pascal: go to definition/body' })
    vim.keymap.set('n', 'gD', vim.lsp.buf.declaration, { buffer = buffer, desc = 'Pascal: go to declaration' })
    vim.keymap.set('n', 'gi', vim.lsp.buf.implementation, { buffer = buffer, desc = 'Pascal: go to implementation' })
    vim.keymap.set('n', 'grn', vim.lsp.buf.rename, { buffer = buffer, desc = 'Pascal: rename symbol' })
    vim.keymap.set({ 'n', 'x' }, 'gra', vim.lsp.buf.code_action, { buffer = buffer, desc = 'Pascal: code actions' })
    pascal_project.attach(client, buffer)
  end,
})

vim.lsp.enable('pascal_lsp')
