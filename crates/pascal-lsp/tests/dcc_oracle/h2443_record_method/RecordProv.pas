unit RecordProv;
interface
type
  TInlineRecord = record
    function GetValue: Integer; inline;
  end;
implementation
uses RecordHelper;
function TInlineRecord.GetValue: Integer;
begin
  Result := HelperValue;
end;
end.
