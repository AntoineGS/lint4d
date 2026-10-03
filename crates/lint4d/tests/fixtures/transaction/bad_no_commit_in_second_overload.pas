unit bad_no_commit_in_second_overload;
interface
implementation
procedure Test(A: Integer); overload;
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

procedure Test(const S: string); overload;
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
