unit ConditionalBody;

interface

type
  TFoo = class
    procedure Hidden;
  end;

implementation

{$IFDEF UNKNOWN_SYMBOL}
procedure TFoo.Hidden;
begin
end;
{$ENDIF}

end.
