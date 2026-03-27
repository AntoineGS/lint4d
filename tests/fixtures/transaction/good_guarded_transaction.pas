unit good_guarded_transaction;
interface
implementation
procedure Test;
var
  trxStartedHere: Boolean;
  trx: TIBTransaction;
begin
  trxStartedHere := not trx.InTransaction;
  if trxStartedHere then
    trx.StartTransaction;
  try
    DoSomeWork;
    if trxStartedHere then
      trx.Commit;
  except
    if trxStartedHere then
      trx.Rollback;
    raise;
  end;
end;
end.
