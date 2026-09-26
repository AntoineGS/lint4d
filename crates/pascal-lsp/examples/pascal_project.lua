local M = {}

function M.attach(client, bufnr)
  local uri = vim.uri_from_bufnr(bufnr)

  local function still_attached()
    return client
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
      and current.selectedProjectUri == expected.selectedProjectUri
      and current.selectionMode == expected.selectionMode
      and vim.deep_equal(current.candidates or {}, expected.candidates or {})
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

  local function select_project(project_uri)
    if not still_attached() then
      return
    end
    request('pascal/selectProject', {
      textDocument = { uri = uri },
      projectUri = project_uri,
    }, function(err, selected)
      if not still_attached() then
        return
      end
      if err then
        vim.notify(err.message, vim.log.levels.ERROR)
      elseif selected then
        notify_warnings(selected.warnings)
        vim.notify('Pascal project selection: ' .. selected.selectionMode)
      end
    end)
  end

  local function browse_repository(context)
    if not require_capability('projectCatalogue', 'repository project browsing') then
      return
    end
    request('pascal/listProjects', { textDocument = { uri = uri } }, function(err, catalogue)
      if not still_attached() then
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
          request_project_context(function(latest_context)
            if not same_project_context(latest_context, current_context) then
              vim.notify('Project context changed while browsing; reopen :PascalProject.', vim.log.levels.WARN)
              return
            end
            local target_uri = choice.projectUri
            request('pascal/selectProject', {
              textDocument = { uri = target_uri },
              projectUri = target_uri,
            }, function(select_err, selected)
              if not still_attached() then
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
              local previous_directory = parent_directory(current_context.selectedProjectUri) or parent_directory(uri)
              if previous_directory == parent_directory(target_uri) or not can_navigate() then
                return
              end
              local destination_uri = type(selected.mainSourceUri) == 'string' and selected.mainSourceUri or target_uri
              if type(selected.mainSourceUri) ~= 'string' then
                vim.notify('Selected project has no usable main source; opening the project file.', vim.log.levels.WARN)
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
            end)
          end)
        end)
      end)
    end)
  end

  local function select_project_picker()
    if not require_capability('projectSelection', 'project selection') then
      return
    end
    request_project_context(function(context)
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
        if choice.kind == 'browse' then
          browse_repository(context)
        elseif choice.kind == 'installation' then
          select_installation()
        else
          select_project(choice.projectUri)
        end
      end)
    end)
  end

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
