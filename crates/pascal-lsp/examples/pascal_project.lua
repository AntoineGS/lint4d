local M = {}

function M.attach(client, bufnr)
  local function select_project()
    local experimental = client.server_capabilities.experimental
    if type(experimental) ~= 'table' or not experimental.projectSelection then
      vim.notify('Installed Pascal LSP does not support project selection; update the server.', vim.log.levels.WARN)
      return
    end
    local uri = vim.uri_from_bufnr(bufnr)
    local function still_attached()
      return vim.api.nvim_buf_is_valid(bufnr)
        and vim.lsp.buf_is_attached(bufnr, client.id)
        and vim.uri_from_bufnr(bufnr) == uri
    end
    local sent = client:request('pascal/projectContext', { textDocument = { uri = uri } }, function(err, context)
      if not still_attached() then
        return
      end
      if err or not context then
        vim.notify(err and err.message or 'Missing project context', vim.log.levels.ERROR)
        return
      end
      for _, warning in ipairs(context.warnings or {}) do
        vim.notify(warning, vim.log.levels.WARN)
      end
      local entries = { { projectUri = vim.NIL, label = 'Automatic' } }
      local selected_is_candidate = false
      for _, candidate in ipairs(context.candidates) do
        local label = vim.fn.fnamemodify(vim.uri_to_fname(candidate), ':t')
        if candidate == context.selectedProjectUri then
          selected_is_candidate = true
          label = label .. ' (current)'
        end
        entries[#entries + 1] = { projectUri = candidate, label = label }
      end
      local prompt = 'Pascal project (' .. context.selectionMode
      if type(context.selectedProjectUri) == 'string' and not selected_is_candidate then
        prompt = prompt .. '; current: ' .. vim.fn.fnamemodify(vim.uri_to_fname(context.selectedProjectUri), ':t')
      end
      prompt = prompt .. ')'
      vim.ui.select(entries, {
        prompt = prompt,
        format_item = function(item)
          return item.label
        end,
      }, function(choice)
        if not choice or not still_attached() then
          return
        end
        local accepted = client:request('pascal/selectProject', {
          textDocument = { uri = uri },
          projectUri = choice.projectUri,
        }, function(select_err, selected)
          if select_err then
            vim.notify(select_err.message, vim.log.levels.ERROR)
          elseif selected then
            vim.notify('Pascal project selection: ' .. selected.selectionMode)
          end
        end, bufnr)
        if not accepted then
          vim.notify('Pascal LSP is no longer available', vim.log.levels.WARN)
        end
      end)
    end, bufnr)
    if not sent then
      vim.notify('Pascal LSP is no longer available', vim.log.levels.WARN)
    end
  end

  vim.api.nvim_buf_create_user_command(bufnr, 'PascalProject', select_project, {
    desc = 'Select Pascal project',
  })
  vim.keymap.set('n', '<leader>wp', select_project, {
    buffer = bufnr,
    desc = 'LSP Select Pascal project',
  })
end

return M
