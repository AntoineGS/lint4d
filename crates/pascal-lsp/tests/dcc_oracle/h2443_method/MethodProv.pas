unit MethodProv;
interface
type
  TInlineClass = class
    function GetValue: Integer; inline;
  end;
implementation
uses MethodHelper;
function TInlineClass.GetValue: Integer;
begin
  Result := HelperValue;
end;
end.
