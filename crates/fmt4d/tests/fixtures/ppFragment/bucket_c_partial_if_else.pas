unit BucketCPartialIfElse;

interface

procedure ReleaseOrder;

implementation

procedure ReleaseOrder;
var
  giftCardReleaseConfig: string;
  giftCardSerialOK: boolean;
begin
  {$IFNDEF NOSF}
  if giftCardReleaseConfig = 'kCALL_STACK_RELEASE' then
    giftCardSerialOK := CheckSerials
  else
  {$ENDIF}
    giftCardSerialOK := True;
end;

end.
