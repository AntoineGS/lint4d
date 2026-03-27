unit bad_nested_start;
interface
implementation
procedure Test;
var
  trx: TIBTransaction;
begin
  trx.StartTransaction;
  try
    DoSomeWork;
    // Starting again without checking InTransaction
    trx.StartTransaction;
    DoMoreWork;
    trx.Commit;
  except
    trx.Rollback;
    raise;
  end;
end;
end.
