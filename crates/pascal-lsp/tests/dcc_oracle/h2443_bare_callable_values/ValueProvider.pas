unit ValueProvider;

interface

function InlineFn: Integer; inline;

implementation

uses InlineHelper;

function InlineFn: Integer;
begin
  Result := HelperValue;
end;

end.
