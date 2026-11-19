unit InlHelper;
interface
function HelperValue: Integer;
type
  TRec = record A: Integer; end;
implementation
function HelperValue: Integer; begin Result := 1; end;
end.
