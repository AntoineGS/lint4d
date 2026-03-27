unit bad_ownership_violation;
interface
implementation
procedure Test;
var
  trx: TIBTransaction;
begin
  if not trx.InTransaction then
    trx.StartTransaction;
  try
    DoSomeWork;
    // Commits unconditionally — but start was conditional (may not own it)
    trx.Commit;
  except
    // Rolls back unconditionally — same ownership problem
    trx.Rollback;
    raise;
  end;
end;
end.
