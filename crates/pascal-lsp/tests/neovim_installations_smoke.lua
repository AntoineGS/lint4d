local REQUEST_TIMEOUT = 10000
local BUSY_ERROR_CODE = -32803
local BUSY_RETRY_DELAY = 50

local function run()
  vim.cmd('filetype on')
  vim.opt.hidden = false

  local pickers = {}
  local notifications = {}
  local original_select = vim.ui.select
  local original_notify = vim.notify
  vim.ui.select = function(items, options, callback)
    pickers[#pickers + 1] = {
      items = items,
      options = options or {},
      callback = callback,
      answered = false,
    }
  end
  vim.notify = function(message, level)
    notifications[#notifications + 1] = { message = tostring(message), level = level }
  end
  dofile(assert(vim.env.PASCAL_LSP_CONFIG))

  local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
  local main_path = root .. '/src/Main.pas'
  local probe_path = root .. '/ambiguous/Probe.pas'
  local main_on_disk = vim.fn.readfile(main_path)

  local function choose(picker, item)
    assert(picker and not picker.answered, 'picker is missing or already answered')
    picker.answered = true
    picker.callback(item)
  end

  local function wait_picker(after, predicate, description)
    local function matching_picker()
      for index = #pickers, after + 1, -1 do
        if predicate(pickers[index]) then
          return pickers[index]
        end
      end
    end
    assert(
      vim.wait(REQUEST_TIMEOUT, function()
        return matching_picker() ~= nil
      end),
      description
    )
    return matching_picker()
  end

  local function find_item(picker, predicate, description)
    for _, item in ipairs(picker.items) do
      if predicate(item) then
        return item
      end
    end
    error('picker is missing ' .. description .. ': ' .. vim.inspect(picker.items))
  end

  local function client_for(bufnr)
    assert(
      vim.wait(REQUEST_TIMEOUT, function()
        return #vim.lsp.get_clients({ bufnr = bufnr, name = 'pascal_lsp' }) == 1
      end),
      'LSP did not attach using the shipped example'
    )
    return vim.lsp.get_clients({ bufnr = bufnr, name = 'pascal_lsp' })[1]
  end

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
    assert(
      vim.wait(REQUEST_TIMEOUT, function()
        return completed
      end),
      method .. ' request timed out'
    )
    assert(not response_error, method .. ' request failed: ' .. vim.inspect(response_error))
    return result
  end

  local function wait_for_response(client, method, params, bufnr, predicate, description)
    local result
    local response_error
    local completed = false
    local function attempt()
      local accepted = client:request(method, params, function(err, response)
        if err and err.code == BUSY_ERROR_CODE then
          vim.defer_fn(attempt, BUSY_RETRY_DELAY)
          return
        end
        if err then
          response_error = err
          completed = true
        elseif predicate(response) then
          result = response
          completed = true
        else
          vim.defer_fn(attempt, BUSY_RETRY_DELAY)
        end
      end, bufnr)
      if not accepted then
        response_error = method .. ' request was not accepted'
        completed = true
      end
    end
    attempt()
    assert(
      vim.wait(REQUEST_TIMEOUT, function()
        return completed
      end),
      description .. ' timed out'
    )
    assert(not response_error, description .. ' failed: ' .. vim.inspect(response_error))
    return result
  end

  local function project_context(client, bufnr, document_uri)
    return request(client, 'pascal/projectContext', {
      textDocument = { uri = document_uri },
    }, bufnr)
  end

  local function installation_context(client, bufnr, project_uri)
    return request(client, 'pascal/installationContext', {
      projectUri = project_uri,
    }, bufnr)
  end

  local function wait_for_project(client, bufnr, document_uri, selected_uri)
    return wait_for_response(
      client,
      'pascal/projectContext',
      {
        textDocument = { uri = document_uri },
      },
      bufnr,
      function(context)
        return context and context.selectedProjectUri == selected_uri
      end,
      'project selection response'
    )
  end

  local function wait_for_installation(client, bufnr, project_uri, selected_id)
    return wait_for_response(
      client,
      'pascal/installationContext',
      {
        projectUri = project_uri,
      },
      bufnr,
      function(context)
        return context and context.selectedInstallationId == selected_id
      end,
      'installation selection response'
    )
  end

  local function open_project_picker(bufnr)
    local before = #pickers
    vim.api.nvim_buf_call(bufnr, function()
      vim.cmd.PascalProject()
    end)
    return wait_picker(before, function(picker)
      return picker.options.kind ~= 'lsp_message'
        and type(picker.options.prompt) == 'string'
        and picker.options.prompt:match('^Pascal project %(') ~= nil
    end, 'PascalProject did not open its explicit picker')
  end

  local function browse_picker_from(project_picker, bufnr)
    local browse = find_item(project_picker, function(item)
      return item.kind == 'browse'
    end, 'Browse repository action')
    local before = #pickers
    choose(project_picker, browse)
    return wait_picker(before, function(picker)
      return picker.options.kind ~= 'lsp_message'
        and type(picker.options.prompt) == 'string'
        and picker.options.prompt:match('^Browse Delphi projects') ~= nil
    end, 'Browse repository did not open the catalogue picker')
  end

  local function repo_item(picker, label)
    return find_item(picker, function(item)
      return item.label == label
    end, label)
  end

  local function cancel_new_automatic_prompts(after)
    for index = after + 1, #pickers do
      local picker = pickers[index]
      if picker.options.kind == 'lsp_message' and not picker.answered then
        choose(picker, nil)
      end
    end
  end

  -- vim.ui.select is replaced before opening the first Pascal document. This
  -- captures Neovim's standard handler for window/showMessageRequest, not a
  -- second automatic-prompt implementation in the shipped helper.
  vim.cmd.edit(vim.fn.fnameescape(probe_path))
  local probe = vim.api.nvim_get_current_buf()
  local client = client_for(probe)
  local probe_uri = vim.uri_from_fname(probe_path)
  local main_uri = vim.uri_from_fname(main_path)
  local alpha_project_uri = vim.uri_from_fname(root .. '/src/Alpha.dproj')
  local ambiguous_alpha_uri = vim.uri_from_fname(root .. '/ambiguous/Alpha.dproj')
  local beta_project_uri = vim.uri_from_fname(root .. '/ambiguous/Beta.dproj')

  local initial_prompt = wait_picker(0, function(picker)
    return picker.options.kind == 'lsp_message'
  end, 'missing standard automatic project prompt')
  assert(#initial_prompt.items == 2, 'automatic prompt must list the two ambiguous projects')
  choose(initial_prompt, nil)
  vim.wait(250)
  local automatic_prompt_count = 0
  for _, picker in ipairs(pickers) do
    if picker.options.kind == 'lsp_message' then
      automatic_prompt_count = automatic_prompt_count + 1
    end
  end
  assert(automatic_prompt_count == 1, 'unchanged ambiguity must produce exactly one automatic prompt')

  local before_unresolved_version = #pickers
  vim.api.nvim_buf_call(probe, function()
    vim.cmd.PascalDelphiVersion()
  end)
  vim.wait(100)
  assert(#pickers == before_unresolved_version, 'installation picker opened for an unresolved project')
  assert(
    vim.tbl_contains(
      vim.tbl_map(function(item)
        return item.message
      end, notifications),
      'Project selection is unresolved; choose a project with :PascalProject first.'
    ),
    'unresolved installation selection did not direct the user to :PascalProject'
  )

  local project_picker = open_project_picker(probe)
  local alpha_choice = find_item(project_picker, function(item)
    return item.projectUri == ambiguous_alpha_uri
  end, 'Alpha.dproj')
  choose(project_picker, alpha_choice)
  wait_for_project(client, probe, probe_uri, ambiguous_alpha_uri)

  local picker_same_directory = open_project_picker(probe)
  local catalogue_same_directory = browse_picker_from(picker_same_directory, probe)
  assert(repo_item(catalogue_same_directory, 'ambiguous/Alpha.dproj (current)'))
  local beta_choice = repo_item(catalogue_same_directory, 'ambiguous/Beta.dproj')
  choose(catalogue_same_directory, beta_choice)
  wait_for_project(client, probe, probe_uri, beta_project_uri)
  assert(vim.api.nvim_get_current_buf() == probe, 'same-directory project browsing navigated away')

  local before_main_open = #pickers
  vim.cmd.edit(vim.fn.fnameescape(main_path))
  local main = vim.api.nvim_get_current_buf()
  assert(main ~= probe, 'opening the installation project did not switch buffers')
  local main_client = client_for(main)
  assert(main_client.id == client.id, 'opening another project started a second LSP client')
  vim.wait(250)
  cancel_new_automatic_prompts(before_main_open)

  local before_install_picker = #pickers
  vim.api.nvim_buf_call(main, function()
    vim.cmd.PascalDelphiVersion()
  end)
  local install_picker = wait_picker(before_install_picker, function(picker)
    return picker.options.kind ~= 'lsp_message'
      and type(picker.options.prompt) == 'string'
      and picker.options.prompt:match('^Delphi installation %(') ~= nil
  end, 'PascalDelphiVersion did not open its explicit picker')
  local undecided_installation = installation_context(client, main, alpha_project_uri)
  assert(
    install_picker.options.prompt:find(undecided_installation.selectionMode, 1, true),
    'installation picker prompt omitted its current selection mode'
  )
  assert(
    find_item(install_picker, function(item)
      return item.installationId == vim.NIL and item.label == 'Automatic'
    end, 'Automatic installation reset'),
    'Automatic must send a null installation ID'
  )
  local version_37 = find_item(install_picker, function(item)
    return item.installationId == '37.0'
  end, 'installation 37.0')
  choose(install_picker, version_37)
  local selected_installation = wait_for_installation(client, main, alpha_project_uri, '37.0')
  assert(selected_installation.selectionMode == 'session', vim.inspect(selected_installation))
  local selected_project_context = project_context(client, main, main_uri)
  assert(
    selected_project_context.selectedInstallationId == '37.0',
    'source project context did not observe the selected profile: ' .. vim.inspect(selected_project_context)
  )

  local sdk_37_provider_path = root .. '/sdk/37.0/source/SdkUnit.pas'
  vim.api.nvim_win_set_cursor(0, { 3, 5 })
  vim.lsp.buf.definition()
  assert(
    vim.wait(REQUEST_TIMEOUT, function()
      return vim.api.nvim_buf_get_name(0) == sdk_37_provider_path
    end),
    'definition did not navigate into the selected SDK root: ' .. vim.inspect(selected_project_context)
  )
  local definition_uri = vim.uri_from_bufnr(vim.api.nvim_get_current_buf())
  local sdk_37_provider = vim.uri_from_fname(root .. '/sdk/37.0/source/SdkUnit.pas')
  assert(definition_uri == sdk_37_provider, 'definition URI was not beneath the selected SDK root')
  vim.api.nvim_set_current_buf(main)

  local before_reset = #pickers
  vim.api.nvim_buf_call(main, function()
    vim.cmd.PascalDelphiVersion()
  end)
  local reset_picker = wait_picker(before_reset, function(picker)
    return picker.options.kind ~= 'lsp_message'
      and type(picker.options.prompt) == 'string'
      and picker.options.prompt:match('^Delphi installation %(') ~= nil
  end, 'installation picker did not reopen for reset')
  assert(reset_picker.options.prompt:find('session', 1, true), 'installation picker omitted the session selection mode')
  assert(find_item(reset_picker, function(item)
    return item.installationId == '37.0' and item.label:find('(current)', 1, true) ~= nil
  end, 'current installation profile'))
  choose(
    reset_picker,
    find_item(reset_picker, function(item)
      return item.installationId == vim.NIL
    end, 'Automatic installation reset')
  )
  wait_for_installation(client, main, alpha_project_uri, vim.NIL)
  local reset_notified = false
  for _, notification in ipairs(notifications) do
    if notification.message:match('^Delphi installation selection: Automatic') then
      reset_notified = true
      break
    end
  end
  assert(reset_notified, 'Automatic reset response was not surfaced')
  vim.wait(100)
  cancel_new_automatic_prompts(0)

  -- Reset with null, then restore the profile before testing browse isolation.
  -- Browsing another directory must not overwrite Alpha's installation.
  local before_restore_picker = #pickers
  vim.api.nvim_buf_call(main, function()
    vim.cmd.PascalDelphiVersion()
  end)
  local restore_picker = wait_picker(before_restore_picker, function(picker)
    return picker.options.kind ~= 'lsp_message'
      and type(picker.options.prompt) == 'string'
      and picker.options.prompt:match('^Delphi installation %(') ~= nil
  end, 'installation picker did not reopen after Automatic reset')
  choose(
    restore_picker,
    find_item(restore_picker, function(item)
      return item.installationId == '37.0'
    end, 'installation 37.0')
  )
  wait_for_installation(client, main, alpha_project_uri, '37.0')

  local unsaved_lines = vim.api.nvim_buf_get_lines(main, 0, -1, false)
  unsaved_lines[#unsaved_lines + 1] = '// unsaved browse preservation check'
  vim.api.nvim_buf_set_lines(main, 0, -1, false, unsaved_lines)
  assert(vim.bo[main].modified, 'fixture source should have an unsaved overlay')

  local picker_other_directory = open_project_picker(main)
  local catalogue_other_directory = browse_picker_from(picker_other_directory, main)
  local other_alpha = repo_item(catalogue_other_directory, 'other/Alpha.dproj')
  local original_browse_request = client.request
  local browse_selection_completed = false
  client.request = function(self, method, params, callback, request_bufnr)
    if method == 'pascal/selectProject' and params.projectUri == other_alpha.projectUri then
      return original_browse_request(self, method, params, function(err, response)
        callback(err, response)
        browse_selection_completed = true
      end, request_bufnr)
    end
    return original_browse_request(self, method, params, callback, request_bufnr)
  end
  choose(catalogue_other_directory, other_alpha)
  local other_main_path = root .. '/other/Alpha.pas'
  assert(
    vim.wait(REQUEST_TIMEOUT, function()
      return browse_selection_completed
    end),
    'repository browse selection callback did not complete'
  )
  client.request = original_browse_request
  assert(
    vim.api.nvim_buf_get_name(0) == other_main_path,
    'repository browsing did not navigate to the selected main source: '
      .. vim.inspect({ current = vim.api.nvim_buf_get_name(0), notifications = notifications })
  )
  local other_main = vim.api.nvim_get_current_buf()
  assert(vim.bo[main].modified, 'browsing another project discarded the unsaved source buffer')
  assert(vim.api.nvim_buf_is_valid(main), 'browsing another project deleted the previous source buffer')
  assert(vim.deep_equal(vim.fn.readfile(main_path), main_on_disk), 'browsing another project saved the unsaved source')
  assert(
    wait_for_installation(client, main, alpha_project_uri, '37.0').selectedInstallationId == '37.0',
    'selecting a project in another directory changed Alpha.dproj installation'
  )

  assert(
    vim.wait(REQUEST_TIMEOUT, function()
      return #vim.lsp.get_clients({ bufnr = other_main, name = 'pascal_lsp' }) == 1
    end),
    'LSP did not attach to the browsed main source'
  )
  local other_client = vim.lsp.get_clients({ bufnr = other_main, name = 'pascal_lsp' })[1]
  assert(other_client.id == client.id, 'repository browse started a second client')

  -- Changing focus after opening the catalogue must not steal it back when the
  -- project-selection response arrives. The unrelated scratch buffer stays dirty.
  vim.bo[other_main].bufhidden = 'hide'
  local focus_picker = open_project_picker(other_main)
  local focus_catalogue = browse_picker_from(focus_picker, other_main)
  local original_request = client.request
  local deliver_focus_response
  client.request = function(self, method, params, callback, request_bufnr)
    if method == 'pascal/selectProject' and params.projectUri == beta_project_uri then
      return original_request(self, method, params, function(err, response)
        deliver_focus_response = function()
          callback(err, response)
        end
      end, request_bufnr)
    end
    return original_request(self, method, params, callback, request_bufnr)
  end
  local unrelated = vim.api.nvim_create_buf(true, false)
  vim.api.nvim_buf_set_name(unrelated, root .. '/Unrelated.txt')
  vim.bo[unrelated].bufhidden = 'hide'
  vim.api.nvim_buf_set_lines(unrelated, 0, -1, false, { 'unsaved unrelated content' })
  vim.api.nvim_set_current_buf(unrelated)
  local unrelated_lines = vim.api.nvim_buf_get_lines(unrelated, 0, -1, false)
  assert(vim.bo[unrelated].modified, 'unrelated buffer should be modified')
  choose(focus_catalogue, repo_item(focus_catalogue, 'ambiguous/Beta.dproj'))
  assert(
    vim.wait(REQUEST_TIMEOUT, function()
      return deliver_focus_response ~= nil
    end),
    'server did not return the browse selection response'
  )
  vim.api.nvim_set_current_buf(unrelated)
  deliver_focus_response()
  client.request = original_request
  assert(vim.api.nvim_get_current_buf() == unrelated, 'late browse response stole focus')
  assert(vim.bo[unrelated].modified, 'late browse response cleared unrelated modifications')
  assert(
    vim.deep_equal(vim.api.nvim_buf_get_lines(unrelated, 0, -1, false), unrelated_lines),
    'late browse response changed unrelated buffer content'
  )
  wait_for_project(client, other_main, probe_uri, beta_project_uri)
  local restored_project = project_context(client, other_main, main_uri)
  assert(restored_project.selectedProjectUri == alpha_project_uri, vim.inspect(restored_project))
  assert(
    installation_context(client, main, alpha_project_uri).selectedInstallationId == '37.0',
    'browsing into another project scope replaced the prior installation selection'
  )

  -- Simulate a late selectProject response after its initiating buffer is deleted.
  vim.api.nvim_set_current_buf(other_main)
  local deleted_picker = open_project_picker(other_main)
  local deleted_catalogue = browse_picker_from(deleted_picker, other_main)
  local delayed_response
  client.request = function(self, method, params, callback, request_bufnr)
    if method == 'pascal/selectProject' then
      delayed_response = callback
      return true
    end
    return original_request(self, method, params, callback, request_bufnr)
  end
  choose(deleted_catalogue, repo_item(deleted_catalogue, 'ambiguous/Beta.dproj'))
  assert(
    vim.wait(5000, function()
      return delayed_response ~= nil
    end),
    'browse did not issue the selection request'
  )
  vim.api.nvim_set_current_buf(unrelated)
  vim.api.nvim_buf_delete(other_main, { force = true })
  delayed_response(nil, {
    mainSourceUri = vim.uri_from_fname(root .. '/src/Main.pas'),
    selectionMode = 'directory',
    warnings = {},
  })
  client.request = original_request
  assert(vim.api.nvim_get_current_buf() == unrelated, 'deleted-buffer response navigated away')

  -- The same guard applies when the client detached from the initiating buffer.
  vim.api.nvim_set_current_buf(main)
  local detached_picker = open_project_picker(main)
  local detached_catalogue = browse_picker_from(detached_picker, main)
  delayed_response = nil
  client.request = function(self, method, params, callback, request_bufnr)
    if method == 'pascal/selectProject' then
      delayed_response = callback
      return true
    end
    return original_request(self, method, params, callback, request_bufnr)
  end
  choose(detached_catalogue, repo_item(detached_catalogue, 'other/Alpha.dproj'))
  assert(
    vim.wait(5000, function()
      return delayed_response ~= nil
    end),
    'browse did not issue the detached-buffer selection request'
  )
  vim.lsp.buf_detach_client(main, client.id)
  delayed_response(nil, {
    mainSourceUri = vim.uri_from_fname(root .. '/other/Alpha.pas'),
    selectionMode = 'directory',
    warnings = {},
  })
  client.request = original_request
  assert(vim.api.nvim_get_current_buf() == main, 'detached-buffer response navigated away')

  local older_capabilities = client.server_capabilities.experimental
  client.server_capabilities.experimental = { projectSelection = true, projectCatalogue = true }
  local before_unsupported = #pickers
  vim.api.nvim_buf_call(main, function()
    vim.cmd.PascalDelphiVersion()
  end)
  vim.wait(100)
  assert(#pickers == before_unsupported, 'older capability must not open the installation picker')
  local warned_about_installation_capability = false
  for _, notification in ipairs(notifications) do
    if notification.message:match('does not support Delphi installation selection') then
      warned_about_installation_capability = true
      break
    end
  end
  assert(warned_about_installation_capability, 'old installation capability warning was not surfaced')
  client.server_capabilities.experimental = older_capabilities

  vim.wait(100)
  cancel_new_automatic_prompts(0)
  vim.ui.select = original_select
  vim.notify = original_notify
  client:stop()
  assert(
    vim.wait(5000, function()
      return client:is_stopped()
    end),
    'server did not shut down'
  )
  io.stdout:write('NEOVIM_INSTALLATION_SELECTION_OK\n')
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
  io.stderr:write(tostring(error_message) .. '\n')
  vim.cmd('cquit 1')
else
  vim.cmd('qa!')
end
