local function run()
  vim.cmd('filetype on')
  vim.opt.hidden = true
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))
  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  vim.lsp.config('pascal_lsp', {
    init_options = { projectFile = 'other/Startup.dproj' },
  })
  local main_path = root .. '/projects/Main.pas'
  local other_path = root .. '/other/Other.pas'
  local b_project_path = root .. '/projects/B.dproj'
  local main_on_disk = vim.fn.readfile(main_path)

  vim.cmd.edit(vim.fn.fnameescape(main_path))
  local main = vim.api.nvim_get_current_buf()
  assert(
    vim.wait(5000, function()
      return #vim.lsp.get_clients({ bufnr = main, name = 'pascal_lsp' }) == 1
    end),
    'LSP did not attach using the shipped example'
  )
  local client = vim.lsp.get_clients({ bufnr = main, name = 'pascal_lsp' })[1]
  assert(client.server_capabilities.experimental.projectSelection, 'project selection capability missing')

  local function context_for(bufnr)
    local result
    local response_error
    local completed = false
    local accepted = client:request('pascal/projectContext', {
      textDocument = { uri = vim.uri_from_bufnr(bufnr) },
    }, function(err, context)
      response_error = err
      result = context
      completed = true
    end, bufnr)
    assert(accepted, 'project context request was not accepted')
    assert(
      vim.wait(5000, function()
        return completed
      end),
      'project context request timed out'
    )
    assert(not response_error, response_error and response_error.message or 'project context failed')
    return result
  end

  local initial_context = context_for(main)
  assert(initial_context.selectionMode == 'configured', 'expected configured startup project')
  assert(initial_context.selectedProjectUri:match('/Startup%.dproj$'), 'startup project was not selected')

  local original_select = vim.ui.select
  local original_notify = vim.notify
  local pickers = {}
  local notifications = {}
  vim.ui.select = function(items, options, callback)
    local picker = { items = items, options = options, callback = callback }
    pickers[#pickers + 1] = picker
  end
  vim.notify = function(message, level)
    notifications[#notifications + 1] = { message = tostring(message), level = level }
  end

  local function open_picker()
    local picker_count = #pickers
    vim.cmd.PascalProject()
    assert(
      vim.wait(5000, function()
        return #pickers > picker_count
      end),
      'PascalProject did not open a picker'
    )
    local picker = pickers[#pickers]
    assert(picker.options.prompt:match('^Pascal project %('), 'picker prompt missing project context')
    return picker
  end

  local function find_item(picker, label)
    for _, item in ipairs(picker.items) do
      if item.label == label then
        return item
      end
    end
    error('picker is missing ' .. label)
  end

  local first_picker = open_picker()
  assert(find_item(first_picker, 'Automatic').projectUri == vim.NIL, 'picker Automatic entry must be null')
  assert(first_picker.options.prompt:match('Startup%.dproj'), 'picker must display the configured current project')
  local configured_project_is_selectable = false
  for _, item in ipairs(first_picker.items) do
    if item.projectUri == initial_context.selectedProjectUri then
      configured_project_is_selectable = true
      break
    end
  end
  assert(
    not configured_project_is_selectable,
    'a configured project outside the candidate list must not become a selectable candidate'
  )
  assert(find_item(first_picker, 'A.dproj'), 'picker is missing A.dproj')
  local b_project = find_item(first_picker, 'B.dproj')
  assert(b_project, 'picker is missing B.dproj')
  first_picker.callback(b_project)

  local selected_context = context_for(main)
  assert(selected_context.selectionMode == 'directory', 'explicit selection must use directory mode')
  assert(selected_context.selectedProjectUri:match('/B%.dproj$'), 'B.dproj was not selected')
  assert(selected_context.selectedProjectUri == b_project.projectUri, 'selected URI does not match the B entry')

  local modified_lines = vim.api.nvim_buf_get_lines(main, 0, -1, false)
  modified_lines[#modified_lines + 1] = '// unsaved project selection test'
  vim.api.nvim_buf_set_lines(main, 0, -1, false, modified_lines)
  assert(vim.bo[main].modified, 'project selection must not clear the modified buffer')

  local before_cancel = context_for(main)
  local cancel_picker = open_picker()
  cancel_picker.callback(nil)
  local after_cancel = context_for(main)
  assert(after_cancel.selectionMode == before_cancel.selectionMode, 'cancellation changed selection mode')
  assert(after_cancel.selectedProjectUri == before_cancel.selectedProjectUri, 'cancellation changed selection')

  local reset_picker = open_picker()
  reset_picker.callback(find_item(reset_picker, 'Automatic'))
  local reset_context = context_for(main)
  assert(reset_context.selectionMode == initial_context.selectionMode, 'Automatic did not restore configured mode')
  assert(reset_context.selectedProjectUri == initial_context.selectedProjectUri, 'Automatic did not clear selection')

  local switch_picker = open_picker()
  vim.cmd.edit(vim.fn.fnameescape(other_path))
  local other = vim.api.nvim_get_current_buf()
  assert(other ~= main, 'buffer switch did not change the current buffer')
  assert(
    vim.wait(5000, function()
      return #vim.lsp.get_clients({ bufnr = other, name = 'pascal_lsp' }) == 1
    end),
    'LSP did not attach to the second buffer'
  )
  switch_picker.callback(find_item(switch_picker, 'B.dproj'))
  local switched_context = context_for(main)
  assert(switched_context.selectionMode == 'directory', 'picker switched the wrong buffer scope')
  assert(switched_context.selectedProjectUri:match('/B%.dproj$'), 'buffer switch lost the selected project')
  assert(vim.api.nvim_get_current_buf() == other, 'selection callback changed the current buffer')

  vim.api.nvim_set_current_buf(main)
  local older_capability = client.server_capabilities.experimental
  client.server_capabilities.experimental = {}
  local picker_count = #pickers
  vim.cmd.PascalProject()
  vim.wait(100)
  assert(#pickers == picker_count, 'older server capability must not open a picker')
  local warned_about_capability = false
  for _, notification in ipairs(notifications) do
    if notification.message:match('does not support project selection') then
      warned_about_capability = true
      break
    end
  end
  assert(warned_about_capability, 'older server capability warning was not surfaced')
  client.server_capabilities.experimental = older_capability

  local deleted_picker = open_picker()
  local stale_b_project
  for _, item in ipairs(deleted_picker.items) do
    if item.projectUri == b_project.projectUri then
      stale_b_project = item
      break
    end
  end
  assert(stale_b_project, 'picker is missing the current B.dproj entry')
  assert(vim.fn.delete(b_project_path) == 0, 'could not remove the stale project candidate')
  deleted_picker.callback(stale_b_project)
  assert(
    vim.wait(5000, function()
      for _, notification in ipairs(notifications) do
        if notification.message:match('not a current candidate') then
          return true
        end
      end
      return false
    end),
    'deleted candidate did not produce a graceful selection warning'
  )
  local after_delete = context_for(main)
  assert(after_delete.selectedProjectUri == vim.NIL, 'deleted candidate was selected')

  assert(vim.bo[main].modified, 'project picker changed the modified state')
  assert(vim.deep_equal(vim.fn.readfile(main_path), main_on_disk), 'project picker saved Main.pas')
  vim.ui.select = original_select
  vim.notify = original_notify
  client:stop()
  assert(
    vim.wait(5000, function()
      return client:is_stopped()
    end),
    'server did not shut down'
  )
  io.stdout:write('NEOVIM_PROJECT_SELECTION_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
