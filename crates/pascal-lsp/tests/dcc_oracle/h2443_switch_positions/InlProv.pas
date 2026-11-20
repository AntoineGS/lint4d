unit InlProv;

interface

function UsesImplUnit: Integer; inline;

implementation

uses InlHelper;

function UsesImplUnit: Integer;
begin
  Result := HelperValue + 1;
end;

end.
