-- Neovim 0.11+. Install pascal-lsp on PATH, or set PASCAL_LSP_BIN to its full path.
-- Load this file from init.lua using dofile('/path/to/examples/neovim.lua').
vim.filetype.add({ extension = { pas = 'pascal', dpr = 'pascal', dpk = 'pascal' } })

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
  on_attach = function(_, buffer)
    vim.keymap.set('n', 'gd', vim.lsp.buf.definition,
      { buffer = buffer, desc = 'Pascal: go to definition/body' })
    vim.keymap.set('n', 'gD', vim.lsp.buf.declaration,
      { buffer = buffer, desc = 'Pascal: go to declaration' })
    vim.keymap.set('n', 'gi', vim.lsp.buf.implementation,
      { buffer = buffer, desc = 'Pascal: go to implementation' })
  end,
})

vim.lsp.enable('pascal_lsp')
