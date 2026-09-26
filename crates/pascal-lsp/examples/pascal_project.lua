local M = {}
local project_intents_by_client = setmetatable({}, { __mode = 'k' })

local function dispatch_project_selection(state, scope, queue, job)
  local completed = false
  local function complete()
    if completed then
      return
    end
    completed = true

    local pending = queue.pending
    queue.pending = nil
    if pending then
      dispatch_project_selection(state, scope, queue, pending)
    else
      queue.in_flight = false
      if state.project_selection_queues[scope] == queue then
        state.project_selection_queues[scope] = nil
      end
    end
  end

  local ok, err = pcall(job, complete)
  if not ok then
    complete()
    vim.notify('Pascal project selection failed: ' .. tostring(err), vim.log.levels.ERROR)
  end
end

local function enqueue_project_selection(state, scope, job)
  local queue = state.project_selection_queues[scope]
  if not queue then
    queue = { in_flight = false, pending = nil }
    state.project_selection_queues[scope] = queue
  end

  if queue.in_flight then
    queue.pending = job
    return
  end

  queue.in_flight = true
  dispatch_project_selection(state, scope, queue, job)
end

function M.attach(client, bufnr)
  local uri = vim.uri_from_bufnr(bufnr)
  local project_intents = project_intents_by_client[client]
  if not project_intents then
    project_intents = {
      sequence = 0,
      scopes = {},
      attachments = {},
      project_selection_queues = {},
    }
    project_intents_by_client[client] = project_intents
  end
  project_intents.attachments[bufnr] = (project_intents.attachments[bufnr] or 0) + 1
  local attachment_generation = project_intents.attachments[bufnr]

  local function is_current_attachment()
    return project_intents.attachments[bufnr] == attachment_generation
  end

  local function still_attached()
    return is_current_attachment()
      and client
      and not client:is_stopped()
      and vim.api.nvim_buf_is_valid(bufnr)
      and vim.lsp.buf_is_attached(bufnr, client.id)
      and vim.uri_from_bufnr(bufnr) == uri
  end

  local function can_navigate()
    return still_attached() and vim.api.nvim_get_current_buf() == bufnr
  end

  local function supports(capability)
    local experimental = client.server_capabilities.experimental
    return type(experimental) == 'table' and experimental[capability] == true
  end

  local function require_capability(capability, feature)
    if supports(capability) then
      return true
    end
    vim.notify('Installed Pascal LSP does not support ' .. feature .. '; update the server.', vim.log.levels.WARN)
    return false
  end

  local function notify_warnings(warnings)
    for _, warning in ipairs(warnings or {}) do
      vim.notify(warning, vim.log.levels.WARN)
    end
  end

  local function request(method, params, callback)
    if not still_attached() then
      return false
    end
    local accepted = client:request(method, params, callback, bufnr)
    if not accepted then
      vim.notify('Pascal LSP is no longer available', vim.log.levels.WARN)
    end
    return accepted
  end

  local function request_project_context(callback)
    request('pascal/projectContext', { textDocument = { uri = uri } }, function(err, context)
      if not still_attached() then
        return
      end
      if err or not context then
        vim.notify(err and err.message or 'Missing project context', vim.log.levels.ERROR)
        return
      end
      notify_warnings(context.warnings)
      callback(context)
    end)
  end

  local function relative_label(project_uri, scope_uri)
    local project_path = vim.uri_to_fname(project_uri)
    local scope_path = type(scope_uri) == 'string' and vim.uri_to_fname(scope_uri) or nil
    if scope_path and vim.fs.relpath then
      local relative = vim.fs.relpath(scope_path, project_path)
      if relative and relative ~= '' then
        return relative
      end
    end
    return vim.fn.fnamemodify(project_path, ':t')
  end

  local function same_project_context(current, expected)
    return current
      and current.scopeUri == expected.scopeUri
      and current.selectedProjectUri == expected.selectedProjectUri
      and current.selectionMode == expected.selectionMode
      and vim.deep_equal(current.candidates or {}, expected.candidates or {})
  end

  local function is_current_project_candidate(context, project_uri)
    if project_uri == vim.NIL then
      return true
    end
    for _, candidate in ipairs(context.candidates or {}) do
      if candidate == project_uri then
        return true
      end
    end
    return false
  end

  local function selection_scope(context)
    return type(context.scopeUri) == 'string' and context.scopeUri or uri
  end

  local function scope_intent(scope)
    return project_intents.scopes[scope] or 0
  end

  local function has_current_intent(scope, intent)
    return scope_intent(scope) == intent
  end

  local function begin_intent(scope)
    project_intents.sequence = project_intents.sequence + 1
    project_intents.scopes[scope] = project_intents.sequence
    return project_intents.sequence
  end

  local function notify_newer_project_intent()
    vim.notify('A newer project selection superseded this picker; reopen :PascalProject.', vim.log.levels.WARN)
  end

  local function parent_directory(project_uri)
    if type(project_uri) ~= 'string' then
      return nil
    end
    local path = vim.fs.normalize(vim.fs.dirname(vim.uri_to_fname(project_uri)))
    if vim.fn.has('win32') == 1 or vim.fn.has('win64') == 1 then
      path = path:lower()
    end
    return path
  end

  local function project_selection_directory(project_uri, context)
    if type(project_uri) == 'string' then
      return parent_directory(project_uri)
    end

    if type(context.selectedProjectUri) == 'string' then
      return parent_directory(context.selectedProjectUri)
    end

    local candidate_directory
    for _, candidate in ipairs(context.candidates or {}) do
      local directory = parent_directory(candidate)
      if directory then
        if candidate_directory and directory ~= candidate_directory then
          return nil
        end
        candidate_directory = directory
      end
    end
    return candidate_directory
  end

  local function selection_belongs_to_directory(context, project_uri, directory)
    if project_uri == vim.NIL then
      return project_selection_directory(project_uri, context) == directory
    end
    return parent_directory(project_uri) == directory and is_current_project_candidate(context, project_uri)
  end

  local function select_installation()
    if not require_capability('installationSelection', 'Delphi installation selection') then
      return
    end
    request_project_context(function(project_context)
      local project_uri = project_context.selectedProjectUri
      if type(project_uri) ~= 'string' then
        vim.notify('Project selection is unresolved; choose a project with :PascalProject first.', vim.log.levels.WARN)
        return
      end
      request('pascal/installationContext', { projectUri = project_uri }, function(err, context)
        if not still_attached() then
          return
        end
        if err or not context then
          vim.notify(err and err.message or 'Missing Delphi installation context', vim.log.levels.ERROR)
          return
        end
        notify_warnings(context.warnings)

        local entries = { { installationId = vim.NIL, label = 'Automatic' } }
        local selected_is_candidate = false
        for _, candidate in ipairs(context.candidates or {}) do
          local label = candidate
          if candidate == context.selectedInstallationId then
            selected_is_candidate = true
            label = label .. ' (current)'
          end
          entries[#entries + 1] = { installationId = candidate, label = label }
        end

        local prompt = 'Delphi installation (' .. (context.selectionMode or 'automatic')
        if type(context.selectedInstallationId) == 'string' and not selected_is_candidate then
          prompt = prompt .. '; current: ' .. context.selectedInstallationId
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
          request_project_context(function(current_project_context)
            if not same_project_context(current_project_context, project_context) then
              vim.notify(
                'Project context changed while the installation picker was open; reopen it.',
                vim.log.levels.WARN
              )
              return
            end

            request(
              'pascal/installationContext',
              { projectUri = project_uri },
              function(fresh_context_err, fresh_context)
                if not still_attached() then
                  return
                end
                if fresh_context_err or not fresh_context then
                  vim.notify(
                    fresh_context_err and fresh_context_err.message or 'Missing Delphi installation context',
                    vim.log.levels.ERROR
                  )
                  return
                end
                if
                  fresh_context.selectionMode ~= context.selectionMode
                  or fresh_context.selectedInstallationId ~= context.selectedInstallationId
                  or not vim.deep_equal(fresh_context.candidates or {}, context.candidates or {})
                then
                  vim.notify(
                    'Delphi installation choices changed while the picker was open; reopen it.',
                    vim.log.levels.WARN
                  )
                  return
                end

                request('pascal/selectInstallation', {
                  projectUri = project_uri,
                  installationId = choice.installationId,
                }, function(select_err, selected)
                  if not still_attached() then
                    return
                  end
                  if select_err then
                    vim.notify(select_err.message, vim.log.levels.ERROR)
                  elseif selected then
                    notify_warnings(selected.warnings)
                    local selection = type(selected.selectedInstallationId) == 'string'
                        and selected.selectedInstallationId
                      or 'Automatic'
                    vim.notify('Delphi installation selection: ' .. selection .. ' (' .. selected.selectionMode .. ')')
                  end
                end)
              end
            )
          end)
        end)
      end)
    end)
  end

  local function select_project(project_uri, expected_context, scope, intent)
    if not still_attached() or not has_current_intent(scope, intent) then
      return
    end
    local directory = project_selection_directory(project_uri, expected_context)
    if not directory then
      vim.notify(
        'Could not determine the project directory for this selection; choose a project candidate first.',
        vim.log.levels.WARN
      )
      return
    end

    enqueue_project_selection(project_intents, directory, function(complete)
      if not still_attached() then
        complete()
        return
      end
      if not has_current_intent(scope, intent) then
        notify_newer_project_intent()
        complete()
        return
      end

      local context_accepted = request(
        'pascal/projectContext',
        { textDocument = { uri = uri } },
        function(context_err, current_context)
          if not still_attached() then
            complete()
            return
          end
          if not has_current_intent(scope, intent) then
            notify_newer_project_intent()
            complete()
            return
          end
          if context_err or not current_context then
            vim.notify(context_err and context_err.message or 'Missing project context', vim.log.levels.ERROR)
            complete()
            return
          end
          notify_warnings(current_context.warnings)
          if not selection_belongs_to_directory(current_context, project_uri, directory) then
            vim.notify(
              'Project choices changed while the selection was queued; reopen :PascalProject.',
              vim.log.levels.WARN
            )
            complete()
            return
          end

          local selection_accepted = request('pascal/selectProject', {
            textDocument = { uri = uri },
            projectUri = project_uri,
          }, function(err, selected)
            complete()
            if not still_attached() then
              return
            end
            if not has_current_intent(scope, intent) then
              notify_newer_project_intent()
              return
            end
            if err then
              vim.notify(err.message, vim.log.levels.ERROR)
            elseif selected then
              notify_warnings(selected.warnings)
              vim.notify('Pascal project selection: ' .. selected.selectionMode)
            end
          end)
          if not selection_accepted then
            complete()
          end
        end
      )
      if not context_accepted then
        complete()
      end
    end)
  end

  local function browse_repository(context, intent_scope, intent)
    if not require_capability('projectCatalogue', 'repository project browsing') then
      return
    end
    request('pascal/listProjects', { textDocument = { uri = uri } }, function(err, catalogue)
      if not still_attached() then
        return
      end
      if not has_current_intent(intent_scope, intent) then
        notify_newer_project_intent()
        return
      end
      if err or not catalogue then
        vim.notify(err and err.message or 'Missing project catalogue', vim.log.levels.ERROR)
        return
      end
      notify_warnings(catalogue.warnings)
      if #catalogue.projects == 0 then
        vim.notify('No Delphi projects found in this repository', vim.log.levels.INFO)
        return
      end

      request_project_context(function(current_context)
        if not has_current_intent(intent_scope, intent) then
          notify_newer_project_intent()
          return
        end
        if not same_project_context(current_context, context) then
          vim.notify('Project context changed while browsing; reopen :PascalProject.', vim.log.levels.WARN)
          return
        end

        local entries = {}
        for _, project in ipairs(catalogue.projects) do
          local label = project.label
          if project.projectUri == current_context.selectedProjectUri then
            label = label .. ' (current)'
          end
          entries[#entries + 1] = {
            kind = 'browse-project',
            projectUri = project.projectUri,
            label = label,
          }
        end
        local prompt = catalogue.complete and 'Browse Delphi projects'
          or 'Browse Delphi projects (incomplete catalogue)'
        vim.ui.select(entries, {
          prompt = prompt,
          format_item = function(item)
            return item.label
          end,
        }, function(choice)
          if not choice or not still_attached() then
            return
          end
          if not has_current_intent(intent_scope, intent) then
            notify_newer_project_intent()
            return
          end
          local selection_intent = begin_intent(intent_scope)
          local target_uri = choice.projectUri
          local target_scope = parent_directory(target_uri)
          if not target_scope then
            vim.notify('Could not determine the browsed project directory; no selection was sent.', vim.log.levels.WARN)
            return
          end
          enqueue_project_selection(project_intents, target_scope, function(complete)
            if not still_attached() or not has_current_intent(intent_scope, selection_intent) then
              if still_attached() then
                notify_newer_project_intent()
              end
              complete()
              return
            end

            local context_accepted = request(
              'pascal/projectContext',
              { textDocument = { uri = target_uri } },
              function(context_err, target_context)
                if not still_attached() or not has_current_intent(intent_scope, selection_intent) then
                  if still_attached() then
                    notify_newer_project_intent()
                  end
                  complete()
                  return
                end
                if context_err or not target_context then
                  vim.notify(
                    context_err and context_err.message or 'Missing project context for browsed project',
                    vim.log.levels.ERROR
                  )
                  complete()
                  return
                end
                notify_warnings(target_context.warnings)
                if not is_current_project_candidate(target_context, target_uri) then
                  vim.notify('Browsed project is no longer a candidate; reopen :PascalProject.', vim.log.levels.WARN)
                  complete()
                  return
                end

                local selection_accepted = request('pascal/selectProject', {
                  textDocument = { uri = target_uri },
                  projectUri = target_uri,
                }, function(select_err, selected)
                  complete()
                  if not still_attached() then
                    return
                  end
                  if not has_current_intent(intent_scope, selection_intent) then
                    notify_newer_project_intent()
                    return
                  end
                  if select_err then
                    vim.notify(select_err.message, vim.log.levels.ERROR)
                    return
                  end
                  if not selected then
                    vim.notify('Missing selected project context', vim.log.levels.ERROR)
                    return
                  end
                  notify_warnings(selected.warnings)

                  -- The project choice is useful in its own directory context, but browsing
                  -- must not switch away from a source file already in that directory.
                  local previous_directory = parent_directory(current_context.selectedProjectUri)
                    or parent_directory(uri)
                  if previous_directory == target_scope or not can_navigate() then
                    return
                  end
                  local function open_destination(destination_uri)
                    if not can_navigate() then
                      return
                    end
                    local destination = vim.uri_to_fname(destination_uri)
                    local ok, switch_error = pcall(vim.api.nvim_cmd, {
                      cmd = 'edit',
                      args = { destination },
                      mods = { hide = true },
                      magic = { file = false, bar = false },
                    }, {})
                    if not ok then
                      vim.notify(
                        'Could not open selected Delphi project source: ' .. tostring(switch_error),
                        vim.log.levels.WARN
                      )
                    end
                  end

                  local function open_project_anchor(reason)
                    vim.notify(reason, vim.log.levels.WARN)
                    open_destination(target_uri)
                  end

                  local destination_uri = selected.mainSourceUri
                  if type(destination_uri) ~= 'string' then
                    open_project_anchor('Selected project has no usable main source; opening the project file.')
                    return
                  end

                  local ownership_request_sequence = project_intents.sequence
                  local ownership_accepted = request(
                    'pascal/projectContext',
                    { textDocument = { uri = destination_uri } },
                    function(context_err, source_context)
                      if not still_attached() or not can_navigate() then
                        return
                      end
                      if not has_current_intent(intent_scope, selection_intent) then
                        notify_newer_project_intent()
                        return
                      end
                      if context_err or not source_context then
                        open_project_anchor(
                          'Could not confirm main source ownership; opening the project file instead.'
                            .. (context_err and ' ' .. context_err.message or '')
                        )
                        return
                      end
                      local source_scope = selection_scope(source_context)
                      if scope_intent(source_scope) > ownership_request_sequence then
                        notify_newer_project_intent()
                        return
                      end

                      local source_intent = scope_intent(source_scope)
                      local recheck_accepted = request(
                        'pascal/projectContext',
                        { textDocument = { uri = destination_uri } },
                        function(recheck_err, rechecked_context)
                          if not still_attached() or not can_navigate() then
                            return
                          end
                          if not has_current_intent(intent_scope, selection_intent) then
                            notify_newer_project_intent()
                            return
                          end
                          if scope_intent(source_scope) ~= source_intent then
                            notify_newer_project_intent()
                            return
                          end
                          if recheck_err or not rechecked_context then
                            open_project_anchor(
                              'Could not confirm current main source ownership; opening the project file instead.'
                                .. (recheck_err and ' ' .. recheck_err.message or '')
                            )
                            return
                          end
                          notify_warnings(rechecked_context.warnings)
                          if
                            selection_scope(rechecked_context) == source_scope
                            and rechecked_context.selectedProjectUri == target_uri
                          then
                            open_destination(destination_uri)
                          else
                            open_project_anchor(
                              'Main source does not resolve to the browsed project in its document context; '
                                .. 'opening the project file instead.'
                            )
                          end
                        end
                      )
                      if not recheck_accepted and can_navigate() then
                        open_project_anchor(
                          'Could not recheck main source ownership; opening the project file instead.'
                        )
                      end
                    end
                  )
                  if
                    not ownership_accepted
                    and can_navigate()
                    and has_current_intent(intent_scope, selection_intent)
                  then
                    open_project_anchor('Could not request main source ownership; opening the project file instead.')
                  end
                end)
                if not selection_accepted then
                  complete()
                end
              end
            )
            if not context_accepted then
              complete()
            end
          end)
        end)
      end)
    end)
  end

  local function select_project_picker()
    if not require_capability('projectSelection', 'project selection') then
      return
    end
    local opened_sequence = project_intents.sequence
    request_project_context(function(context)
      local scope = selection_scope(context)
      if scope_intent(scope) > opened_sequence then
        notify_newer_project_intent()
        return
      end
      local picker_intent = scope_intent(scope)
      local entries = { { kind = 'project', projectUri = vim.NIL, label = 'Automatic' } }
      local selected_is_candidate = false
      for _, candidate in ipairs(context.candidates or {}) do
        local label = relative_label(candidate, context.scopeUri)
        if candidate == context.selectedProjectUri then
          selected_is_candidate = true
          label = label .. ' (current)'
        end
        entries[#entries + 1] = { kind = 'project', projectUri = candidate, label = label }
      end
      if supports('projectCatalogue') then
        entries[#entries + 1] = { kind = 'browse', label = 'Browse repository' }
      end
      if supports('installationSelection') then
        entries[#entries + 1] = { kind = 'installation', label = 'Select Delphi installation' }
      end

      local prompt = 'Pascal project (' .. (context.selectionMode or 'automatic')
      if type(context.selectedProjectUri) == 'string' and not selected_is_candidate then
        prompt = prompt .. '; current: ' .. relative_label(context.selectedProjectUri, context.scopeUri)
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
        if not has_current_intent(scope, picker_intent) then
          notify_newer_project_intent()
          return
        end
        if choice.kind == 'browse' then
          browse_repository(context, scope, begin_intent(scope))
        elseif choice.kind == 'installation' then
          select_installation()
        elseif choice.kind == 'project' then
          select_project(choice.projectUri, context, scope, begin_intent(scope))
        end
      end)
    end)
  end

  pcall(vim.api.nvim_buf_del_user_command, bufnr, 'PascalProject')
  pcall(vim.api.nvim_buf_del_user_command, bufnr, 'PascalDelphiVersion')
  vim.api.nvim_buf_create_user_command(bufnr, 'PascalProject', select_project_picker, {
    desc = 'Select Pascal project',
  })
  vim.api.nvim_buf_create_user_command(bufnr, 'PascalDelphiVersion', select_installation, {
    desc = 'Select Delphi installation for the current project',
  })
  vim.keymap.set('n', '<leader>wp', select_project_picker, {
    buffer = bufnr,
    desc = 'LSP Select Pascal project',
  })
end

return M
