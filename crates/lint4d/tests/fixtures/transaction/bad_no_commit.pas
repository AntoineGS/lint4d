unit bad_no_commit;
interface
implementation
procedure Test;
var
  trx: TIBTransaction;
begin
  trx.StartTransaction;
  try
    DoSomeWork;
    // No commit anywhere on the normal path
  except
    trx.Rollback;
    raise;
  end;
end;
end.
