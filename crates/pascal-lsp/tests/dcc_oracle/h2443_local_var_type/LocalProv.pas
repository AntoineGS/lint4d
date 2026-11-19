unit LocalProv;
interface
function LocalRecordValue: Integer; inline;
implementation
uses LocalHelper;
function LocalRecordValue: Integer;
var R: TRecordFromHelper;
begin
  R.Value := 5;
  Result := R.Value;
end;
end.
