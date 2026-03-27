unit bad_no_rollback;
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

  DoSomeWork;

  if trxStartedHere then
    trx.Commit;
  // No try/except, no rollback — transaction left dangling on exception
end;
end.
